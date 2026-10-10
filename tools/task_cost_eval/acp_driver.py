"""One-turn, line-delimited ACP client for paired local Distill binaries (POSIX).

Wire contracts: agent-client-protocol 0.10.4 and Distill's headless.rs,
model_switch.rs and session_ultracode.rs. Extension methods have a wire `_` prefix.
No authentication RPC: the isolated profile supplies the model's existing OAuth.
"""
from __future__ import annotations

import json
import os
import select
import signal
import subprocess
import time

from .runner import _terminate_owned_group

ULTRACODE_METHOD = '_x.ai/session/ultracode/set'
SHUTDOWN_GRACE = 2.0


class AcpError(RuntimeError):
    pass


class OwnedTree:
    """Track descendants, including separate process groups, while the peer runs."""
    def __init__(self, process):
        self.process = process
        self.pids = {process.pid}
        self.live = set(self.pids)
        self.checked = 0.0

    def refresh(self, force=False, deadline=None):
        if deadline is not None and time.monotonic() >= deadline:
            raise TimeoutError('ACP process snapshot deadline exceeded')
        if not force and time.monotonic() - self.checked < 0.1:
            return
        # A transient scheduler delay is not an inference deadline. Retry once;
        # persistent failure still propagates, including during forced cleanup.
        for attempt in range(2):
            remaining = 5 if deadline is None else min(5, deadline - time.monotonic())
            if remaining <= 0:
                raise TimeoutError('ACP process snapshot deadline exceeded')
            command = ['ps', '-A', '-o', 'pid=,ppid=,pgid=,stat=']
            snapshot = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                try:
                    remaining = 5 if deadline is None else max(0, min(5, deadline - time.monotonic()))
                    stdout, stderr = snapshot.communicate(timeout=remaining)
                except subprocess.TimeoutExpired:
                    snapshot.kill()
                    # subprocess.run's timeout handler waits without a bound.
                    try:
                        snapshot.wait(timeout=None if deadline is None else max(0, deadline - time.monotonic()))
                    except subprocess.TimeoutExpired:
                        pass
                    raise
                if snapshot.returncode:
                    raise subprocess.CalledProcessError(snapshot.returncode, command, stdout, stderr)
                break
            except subprocess.TimeoutExpired:
                if attempt:
                    raise
            finally:
                snapshot.stdout.close()
                snapshot.stderr.close()
        if deadline is not None and time.monotonic() >= deadline:
            raise TimeoutError('ACP process snapshot deadline exceeded')
        rows = [line.split() for line in stdout.splitlines()]
        descendants = {int(pid) for pid, _, group, _ in rows if int(group) == self.process.pid}
        descendants.update(self.pids.intersection(int(row[0]) for row in rows))
        while True:
            found = {int(pid) for pid, parent, _, _ in rows if int(parent) in descendants}
            if found <= descendants:
                break
            descendants.update(found)
        live = {int(pid) for pid, _, _, state in rows if int(pid) in descendants and not state.startswith('Z')}
        if deadline is not None and time.monotonic() >= deadline:
            raise TimeoutError('ACP process snapshot deadline exceeded')
        self.pids = descendants
        self.live = live
        self.checked = time.monotonic()

    def stop(self, deadline=None):
        # Refresh before signalling: children can have their own session/process group.
        snapshot_error = None
        try:
            self.refresh(force=True, deadline=deadline)
        except (OSError, subprocess.SubprocessError, TimeoutError) as error:
            snapshot_error = error
        for pid in (self.pids if snapshot_error else self.live) - {self.process.pid}:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        _terminate_owned_group(self.process, deadline=deadline)
        # The leader may already have exited; its remaining group still belongs to us.
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for pid in self.pids - {self.process.pid}:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if snapshot_error:
            raise snapshot_error
        stop_deadline = deadline if deadline is not None else time.monotonic() + SHUTDOWN_GRACE
        while True:
            if time.monotonic() >= stop_deadline:
                return False
            self.refresh(force=True, deadline=deadline)
            if not self.live:
                return True
            for pid in self.live:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if time.monotonic() >= stop_deadline:
                return False
            time.sleep(min(0.02, max(0, stop_deadline - time.monotonic())))


class _Client:
    def __init__(self, process, tree, cwd, deadline, stdout, transcript):
        self.process, self.tree, self.cwd, self.deadline = process, tree, cwd.resolve(), deadline
        self.stdout, self.transcript = stdout, transcript
        self.buffer = b''
        self.sequence = 0
        self.session = None
        self.receipts = {}
        self.tools = {}
        self.pending = set()
        self.finished = set()
        os.set_blocking(process.stdout.fileno(), False)
        os.set_blocking(process.stdin.fileno(), False)

    def log(self, direction, message):
        self.transcript.write(json.dumps({'direction': direction, 'message': message}) + '\n')
        self.transcript.flush()

    def send(self, message, deadline=None):
        data = (json.dumps({'jsonrpc': '2.0', **message}) + '\n').encode()
        deadline = self.deadline if deadline is None else deadline
        while data:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('ACP write deadline exceeded')
            if select.select([], [self.process.stdin], [], min(remaining, 0.1))[1]:
                try:
                    data = data[os.write(self.process.stdin.fileno(), data):]
                except BlockingIOError:
                    continue
            self.tree.refresh(deadline=deadline)
        self.log('sent', {'jsonrpc': '2.0', **message})

    def receive(self, block=True):
        while True:
            self.tree.refresh(deadline=self.deadline)
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('ACP response/completion deadline exceeded')
            if b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                message = json.loads(line)
                if not isinstance(message, dict) or message.get('jsonrpc') != '2.0':
                    raise AcpError('Invalid ACP JSON-RPC message')
                self.log('received', message)
                # app.rs injects these watcher requests into the shared stream.
                # They acknowledge reloads, not this client's outstanding call.
                if message.get('id') in ('skills-reload', 'workflows-reload'):
                    result = message.get('result')
                    payload = result.get('result') if isinstance(result, dict) else None
                    if (set(message) != {'jsonrpc', 'id', 'result'}
                            or not isinstance(result, dict) or set(result) != {'result'}
                            or not isinstance(payload, dict) or set(payload) != {'reloaded'}
                            or type(payload['reloaded']) is not int or payload['reloaded'] < 0):
                        raise AcpError(f'Invalid internal reload acknowledgement: {message["id"]}')
                    continue
                return message
            wait = block or bool(self.buffer)
            ready = select.select([self.process.stdout], [], [], min(remaining, 0.1) if wait else 0)[0]
            if not ready:
                if not wait:
                    return None
                if self.process.poll() is not None:
                    raise AcpError('ACP peer exited before the expected response/tool completion')
                continue
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                if not block and not self.buffer:
                    return None
                raise AcpError('ACP EOF before the expected response/tool completion')
            self.stdout.write(chunk)
            self.stdout.flush()
            self.buffer += chunk

    def request(self, method, params):
        self.sequence += 1
        identity = f'benchmark-{self.sequence}'
        self.send({'id': identity, 'method': method, 'params': params})
        while True:
            message = self.receive()
            if 'method' in message:
                self.handle(message)
                continue
            if message.get('id') != identity:
                raise AcpError(f'Unexpected ACP response ID during {method}')
            self.receipts[method] = message
            if 'error' in message:
                raise AcpError(f'{method}: {json.dumps(message["error"])}')
            if not isinstance(message.get('result'), dict):
                raise AcpError(f'{method}: missing object result')
            return message['result']

    def permission(self, params):
        tool = params.get('toolCall', {})
        tool = {**self.tools.get(tool.get('toolCallId'), {}), **tool}
        kind = tool.get('kind')
        locations = tool.get('locations', [])
        paths = [location.get('path') for location in locations]
        raw = tool.get('rawInput') or {}
        if isinstance(raw, dict):
            paths += [raw[key] for key in ('cwd', 'workdir', 'path', 'file_path', 'target_directory') if key in raw]
        allowed = (params.get('sessionId') == self.session
                   and kind in ('read', 'edit', 'delete', 'move', 'search', 'execute')
                   and not params.get('_meta')
                   and (bool(paths) or kind == 'execute'))
        for path in paths:
            if not isinstance(path, str) or not path or not (self.cwd / path).resolve().is_relative_to(self.cwd):
                allowed = False
        # A shell command uses the session cwd; this is a permission policy, not a shell sandbox.
        option = next((o for o in params.get('options', [])
                       if o.get('kind') == 'allow_once' and isinstance(o.get('optionId'), str)), None)
        if allowed and option:
            return {'outcome': {'outcome': 'selected', 'optionId': option['optionId']}}
        return {'outcome': {'outcome': 'cancelled'}}

    def handle(self, message):
        method, params = message['method'], message.get('params', {})
        # Native headless policy replies do not parse interaction parameters.
        if 'id' in message and method == '_x.ai/exit_plan_mode':
            self.send({'id': message['id'], 'result': {'outcome': 'approved'}})
            return
        if not isinstance(params, dict):
            raise AcpError(f'{method}: invalid params')
        if 'id' in message:
            if method == 'session/request_permission':
                reply = self.permission(params)
                self.send({'id': message['id'], 'result': reply})
                if reply['outcome']['outcome'] == 'cancelled':
                    raise AcpError('Permission outside the supported fixture policy')
            elif method in ('_x.ai/ask_user_question', '_x.ai/mcp/elicit'):
                outcome = 'cancelled' if method == '_x.ai/ask_user_question' else 'cancel'
                self.send({'id': message['id'], 'result': {'outcome': outcome}})
            else:
                self.send({'id': message['id'], 'error': {'code': -32601, 'message': 'Unsupported benchmark client request'}})
                raise AcpError(f'Unsupported ACP client request: {method}')
            return
        update = params.get('update', {})
        if not isinstance(update, dict):
            raise AcpError(f'{method}: invalid update')
        tag = update.get('sessionUpdate')
        key, done = None, False
        if method == 'session/update' and tag in ('tool_call', 'tool_call_update'):
            identity = update.get('toolCallId')
            if not isinstance(identity, str):
                raise AcpError('Tool update missing toolCallId')
            tool = self.tools.setdefault(identity, {})
            tool.update(update)
            key = ('tool', identity)
            done = tool.get('status', 'pending') in ('completed', 'failed')
        elif method in ('_x.ai/session/update', '_x.ai/session_notification') and tag in ('subagent_spawned', 'subagent_finished'):
            if not isinstance(update.get('subagent_id'), str):
                raise AcpError('Subagent update missing subagent_id')
            key = ('subagent', update['subagent_id'], update.get('attempt_id'))
            done = tag == 'subagent_finished'
        elif method in ('_x.ai/task_backgrounded', '_x.ai/task_completed'):
            done = method == '_x.ai/task_completed'
            if tag != ('task_completed' if done else 'task_backgrounded'):
                raise AcpError('Mismatched background task update')
            identity = update.get('task_snapshot', {}).get('task_id') if done else update.get('task_id')
            if not isinstance(identity, (str, int)):
                raise AcpError('Background update missing task_id')
            key = ('task', str(identity))
        if key is not None:
            if done:
                self.pending.discard(key)
                self.finished.add(key)
            elif key not in self.finished:
                self.pending.add(key)


def run_acp(command, *, cwd, environment, prompt, model, effort, ultracode, timeout, output):
    """Return receipts and failure state; always stop the owned tree before returning."""
    result = {'session': None, 'setup_confirmed': False, 'completed': False, 'timed_out': False, 'error': None,
              'exit_code': None, 'cleanup_complete': False}
    process = client = tree = None
    deadline = time.monotonic() + timeout
    inference_deadline = deadline - min(SHUTDOWN_GRACE, timeout / 2)
    with (output / 'stdout.jsonl').open('wb') as stdout, (output / 'stderr.txt').open('w') as stderr, \
            (output / 'acp-transcript.jsonl').open('w') as transcript:
        try:
            process = subprocess.Popen(command, cwd=cwd, env=environment, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=stderr, start_new_session=True, bufsize=0)
            tree = OwnedTree(process)
            tree.refresh(force=True, deadline=inference_deadline)
            client = _Client(process, tree, cwd, inference_deadline, stdout, transcript)
            init = client.request('initialize', {
                'protocolVersion': 1, 'clientCapabilities': {'fs': {'readTextFile': False, 'writeTextFile': False}, 'terminal': False},
                '_meta': {'clientType': 'grok-shell', 'startupHints': {
                    'nonInteractive': True, 'skipGitStatus': True, 'skipProjectLayout': True}},
            })
            if type(init.get('protocolVersion')) is not int or init['protocolVersion'] != 1:
                raise AcpError('Unsupported ACP protocolVersion')
            session = client.request('session/new', {'cwd': str(cwd.resolve()), 'mcpServers': [],
                                                    '_meta': {'sessionKind': 'headless'}}).get('sessionId')
            if not isinstance(session, str) or not session:
                raise AcpError('session/new did not return a sessionId')
            client.session = result['session'] = session
            auto = effort in (None, 'auto')
            meta = {'reasoningEffortAuto': auto}
            if not auto:
                meta['reasoningEffort'] = effort
            selected = client.request('session/set_model', {'sessionId': session, 'modelId': model, '_meta': meta})
            applied = selected.get('_meta', {})
            if (not isinstance(applied, dict) or applied.get('canonicalModelId') != model or applied.get('reasoningEffortAuto') is not auto
                    or (not auto and applied.get('reasoningEffort') != effort)):
                raise AcpError('session/set_model did not confirm the requested model/effort')
            activation = client.request(ULTRACODE_METHOD, {'sessionId': session, 'enabled': ultracode})
            if activation.get('enabled') is not ultracode:
                raise AcpError(f'UltraCode activation did not confirm enabled={ultracode}')
            result['setup_confirmed'] = True
            response = client.request('session/prompt', {'sessionId': session,
                'prompt': [{'type': 'text', 'text': prompt}], '_meta': {'screenMode': 'headless'}})
            if response.get('stopReason') != 'end_turn':
                raise AcpError(f'Prompt did not finish with end_turn: {response.get("stopReason")}')
            # Drain buffered notifications too: a prompt response can precede tool completion.
            while True:
                message = client.receive(block=bool(client.pending))
                if message is None:
                    break
                if 'method' not in message:
                    raise AcpError('Unexpected response after prompt completion')
                client.handle(message)
            tree.refresh(force=True, deadline=inference_deadline)
            process.stdin.close()
            while process.poll() is None:
                tree.refresh(deadline=inference_deadline)
                if time.monotonic() >= inference_deadline:
                    raise TimeoutError('ACP shutdown deadline exceeded')
                time.sleep(min(0.02, max(0, inference_deadline - time.monotonic())))
            if process.returncode != 0:
                raise AcpError(f'ACP process exited {process.returncode}')
            result['completed'] = True
        except (AcpError, OSError, ValueError, subprocess.SubprocessError, TimeoutError) as error:
            result['error'] = str(error)
            result['timed_out'] = isinstance(error, (TimeoutError, subprocess.TimeoutExpired))
        finally:
            if client is not None:
                if not result['completed'] and client.session and not process.stdin.closed:
                    try:
                        client.send({'method': 'session/cancel', 'params': {'sessionId': client.session}},
                                    deadline=min(deadline, time.monotonic() + 0.2))
                    except (OSError, TimeoutError, subprocess.SubprocessError):
                        pass
                result['model_receipt'] = client.receipts.get('session/set_model')
                result['activation_receipt'] = client.receipts.get(ULTRACODE_METHOD)
                result['prompt_receipt'] = client.receipts.get('session/prompt')
            if process is not None:
                if not process.stdin.closed:
                    process.stdin.close()
                try:
                    result['cleanup_complete'] = (tree or OwnedTree(process)).stop(deadline=deadline)
                except (OSError, subprocess.SubprocessError, TimeoutError) as error:
                    cleanup_error = f'Cannot verify owned tree cleanup: {error}'
                    result['error'] = f'{result["error"]}; {cleanup_error}' if result['error'] else cleanup_error
                    result['timed_out'] |= isinstance(error, (TimeoutError, subprocess.TimeoutExpired))
                process.stdout.close()
                result['exit_code'] = process.returncode
            else:
                result['cleanup_complete'] = True
            if time.monotonic() > deadline:
                result['timed_out'] = True
                result['error'] = result['error'] or 'ACP tree termination exceeded the run deadline'
            result['completed'] &= result['cleanup_complete'] and not result['error']
            (output / 'acp-result.json').write_text(json.dumps(result, indent=2) + '\n')
    return result
