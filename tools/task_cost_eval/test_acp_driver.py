import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from . import acp_driver
from . import runner


# A real stdio process, with no provider or credential access. It deliberately
# returns a session ID the caller did not choose and completes tools after prompt.
PEER = r'''
import json, os, signal, subprocess, sys, time
from pathlib import Path

scenario = SCENARIO
root = Path.cwd()
(root / 'peer-env.json').write_text(json.dumps({key: os.environ[key] for key in (
    'GROK_SUBAGENTS_MAX_DEPTH', 'GROK_SESSION_SUMMARY_MODEL', 'GROK_REASONING_EFFORT') if key in os.environ}))
def send(message):
    print(json.dumps({'jsonrpc': '2.0', **message}), flush=True)
def update(tag, **values):
    send({'method': 'session/update', 'params': {'sessionId': 'runtime-session',
          'update': {'sessionUpdate': tag, **values}}})
def extension(method, tag, **values):
    send({'method': method, 'params': {'sessionId': 'runtime-session',
          'update': {'sessionUpdate': tag, **values}}})
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'session/cancel':
        (root / 'cancelled').write_text('yes')
        continue
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {}, 'authMethods': []}
    elif method == 'session/new':
        if scenario in ('stop-feedback', 'stop-feedback-scope'):
            assert request['params']['_meta'] == {'sessionKind': 'headless', 'x.ai/hooks': {
                'stop': [{'hookCallbackIds': ['benchmark-root-stop']}]}}
        result = {'sessionId': 'runtime-session'}
    elif method == 'session/set_model':
        params = request['params']
        result = {'_meta': {'canonicalModelId': params['modelId'],
            'reasoningEffort': params['_meta'].get('reasoningEffort'),
            'reasoningEffortAuto': params['_meta']['reasoningEffortAuto']}}
        if scenario == 'wrong-model':
            result['_meta']['canonicalModelId'] = 'other-model'
        if scenario == 'wrong-effort':
            result['_meta']['reasoningEffort'] = 'high'
    elif method == '_x.ai/session/ultracode/set':
        if scenario == 'activation-error':
            send({'id': request['id'], 'error': {'code': -32601, 'message': 'unsupported'}})
            continue
        if scenario in ('timeout', 'eof', 'stubborn'):
            # A writer in a separate session proves cleanup is not just wait(parent).
            code = "import pathlib,signal,time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nwhile True:\n pathlib.Path('pulse').write_text(str(time.time()))\n time.sleep(.02)"
            child = subprocess.Popen([sys.executable, '-c', code], start_new_session=True)
            (root / 'child.pid').write_text(str(child.pid))
            time.sleep(.25)
            if scenario == 'eof':
                sys.exit(0)
            if scenario == 'stubborn':
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                time.sleep(60)
            # Consume cancellation but never answer the activation request.
            continue
        result = {'enabled': request['params']['enabled']}
        if scenario == 'activation-false':
            result = {'enabled': False}
        if scenario == 'activation-missing':
            result = {}
        if scenario == 'activation-null':
            result = None
    elif method == 'session/prompt':
        (root / 'prompt-seen').write_text(request['params']['prompt'][0]['text'])
        if scenario in ('stop-feedback', 'stop-feedback-scope'):
            envelope = {'hookCallbackId': 'benchmark-root-stop', 'sessionId': 'runtime-session',
                        'hookEventName': 'stop', 'reason': 'end_turn', 'stopHookActive': False,
                        'cwd': str(root), 'workspaceRoot': str(root), 'timestamp': '2026-10-10T00:00:00Z'}
            calls = []
            if scenario == 'stop-feedback-scope':
                send({'method': '_x.ai/hooks/event', 'params': envelope})
                calls.extend([{**envelope, 'sessionId': 'child-session'},
                              {**envelope, 'hookCallbackId': 'other-callback'},
                              {**envelope, 'hookEventName': 'subagent_stop'},
                              {**envelope, 'reason': 'cancelled'}])
            calls.extend([envelope, {**envelope, 'stopHookActive': True}])
            replies = []
            for index, params in enumerate(calls):
                identity = f'hook-{index}'
                send({'id': identity, 'method': '_x.ai/hooks/run', 'params': params})
                reply = json.loads(sys.stdin.readline())
                expected = ({'decision': 'deny', 'systemMessage': 'Please check whether the requested work is complete.'}
                            if index == len(calls) - 2 else {'decision': 'continue'})
                assert reply == {'jsonrpc': '2.0', 'id': identity, 'result': expected}, reply
                replies.append(reply)
            (root / 'hook-responses.json').write_text(json.dumps(replies))
        if scenario == 'exit-plan-mode':
            send({'id': 'plan', 'method': '_x.ai/exit_plan_mode', 'params': {
                'sessionId': 'runtime-session', 'toolCallId': 'plan-1', 'planContent': '# Plan'}})
            reply = json.loads(sys.stdin.readline())
            (root / 'plan-response.json').write_text(json.dumps(reply))
            assert reply == {'jsonrpc': '2.0', 'id': 'plan', 'result': {'outcome': 'approved'}}
        if scenario == 'unsupported-request':
            send({'id': 99, 'method': 'terminal/create', 'params': {'sessionId': 'runtime-session'}})
            (root / 'permission-response.json').write_text(sys.stdin.readline())
            continue
        location = '/outside-fixture/file.py' if scenario == 'outside-permission' else str(root / 'file.py')
        update('tool_call', toolCallId='tool-1', title='Edit fixture', kind='edit',
               status='in_progress', locations=[{'path': location}])
        send({'id': 99, 'method': 'session/request_permission', 'params': {
            'sessionId': 'runtime-session', 'toolCall': {'toolCallId': 'tool-1'},
            'options': [{'optionId': 'yes-once', 'kind': 'allow_once', 'name': 'Yes'}]}})
        reply = json.loads(sys.stdin.readline())
        (root / 'permission-response.json').write_text(json.dumps(reply))
        extension('_x.ai/task_backgrounded', 'task_backgrounded', task_id=7)
        extension('_x.ai/session/update', 'subagent_spawned', subagent_id='child', attempt_id='attempt')
        if scenario == 'reload-acks':
            send({'id': 'skills-reload', 'result': {'result': {'reloaded': 1}}})
        if scenario == 'reload-malformed':
            send({'id': 'skills-reload', 'result': {'result': {'reloaded': True}}})
        if scenario == 'reload-rpc-error':
            send({'id': 'skills-reload', 'error': {'code': -32603, 'message': 'reload failed'}})
        if scenario == 'prompt-wrong-id':
            send({'id': 'benchmark-4', 'result': {'stopReason': 'end_turn'}})
        if scenario == 'missing-stop':
            sys.exit(0)
        result = {'stopReason': 'refusal' if scenario == 'negative-stop' else 'end_turn'}
        send({'id': request['id'], 'result': result})
        if scenario == 'reload-acks':
            send({'id': 'workflows-reload', 'result': {'result': {'reloaded': 0}}})
        if scenario == 'reload-unknown':
            send({'id': 'other-reload', 'result': {'result': {'reloaded': 1}}})
        if scenario == 'reload-error':
            send({'id': 'workflows-reload', 'result': {'result': {'reloaded': 1}, 'error': 'reload failed'}})
        time.sleep(.08)
        update('tool_call_update', toolCallId='tool-1', status='completed')
        extension('_x.ai/task_completed', 'task_completed', task_snapshot={'task_id': 7})
        extension('_x.ai/session/update', 'subagent_finished', subagent_id='child', attempt_id='attempt')
        (root / 'tools-completed').write_text('yes')
        continue
    else:
        raise RuntimeError(method)
    send({'id': request['id'], 'result': result})
'''


def fake_peer(root, scenario='success'):
    path = root / 'peer.py'
    path.write_text(f'#!{sys.executable}\nSCENARIO = {scenario!r}\n' + PEER)
    path.chmod(0o700)
    return path


def drive(root, scenario='success', timeout=3, ultracode=True, effort='medium', stop_feedback=None):
    peer = fake_peer(root, scenario)
    return acp_driver.run_acp([str(peer)], cwd=root, environment=os.environ.copy(), prompt='Change fixture',
                             model='chatgpt/gpt-6.1-sol', effort=effort, ultracode=ultracode,
                             timeout=timeout, output=root, stop_feedback=stop_feedback)


class AcpDriverTest(unittest.TestCase):
    def test_stop_receipt_tracks_full_write_before_refresh_or_logging_failure(self):
        params = {'sessionId': 'runtime-session', 'hookCallbackId': acp_driver.STOP_CALLBACK_ID,
                  'hookEventName': 'stop', 'reason': 'end_turn'}
        message = {'id': 'hook-0', 'method': acp_driver.HOOK_RUN_METHOD, 'params': params}
        reply = {'decision': 'deny', 'systemMessage': 'Check completion.'}
        wire = (json.dumps({'jsonrpc': '2.0', 'id': 'hook-0', 'result': reply}) + '\n').encode()
        for failure in ('refresh', 'logging', 'partial', 'write'):
            with self.subTest(failure=failure):
                process, tree, transcript = mock.Mock(), mock.Mock(), mock.Mock()
                transmitted = bytearray()

                def write(fd, data):
                    if failure == 'write':
                        raise OSError('injected failure')
                    chunk = data[:1] if failure == 'partial' else data
                    transmitted.extend(chunk)
                    return len(chunk)

                if failure in ('refresh', 'partial'):
                    tree.refresh.side_effect = OSError('injected failure')
                if failure == 'logging':
                    transcript.write.side_effect = OSError('injected failure')
                with mock.patch.object(acp_driver.os, 'set_blocking'), \
                        mock.patch.object(acp_driver.select, 'select', return_value=([], [process.stdin], [])), \
                        mock.patch.object(acp_driver.os, 'write', side_effect=write):
                    client = acp_driver._Client(process, tree, Path.cwd(), time.monotonic() + 3,
                                                mock.Mock(), transcript, 'Check completion.')
                    client.session = 'runtime-session'
                    with self.assertRaisesRegex(OSError, 'injected failure'):
                        client.handle(message)
                    sent = failure in ('refresh', 'logging')
                    self.assertEqual(bytes(transmitted), wire if sent else wire[:1] if failure == 'partial' else b'')
                    self.assertEqual(client.stop_feedback_sent, sent)
                    self.assertEqual(client.stop_hook_receipts, [
                        {'request': {'id': 'hook-0', **params}, 'response': reply, 'feedback_sent': True}
                    ] if sent else [])
                    if sent:
                        tree.refresh.side_effect = transcript.write.side_effect = None
                        transmitted.clear()
                        client.handle({**message, 'id': 'hook-1'})
                        self.assertEqual(json.loads(transmitted)['result'], {'decision': 'continue'})
                        self.assertFalse(client.stop_hook_receipts[-1]['feedback_sent'])

    def test_opt_in_root_stop_feedback_is_sent_once_then_completes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            feedback = 'Please check whether the requested work is complete.'
            result = drive(root, 'stop-feedback', stop_feedback=feedback)
            self.assertTrue(result['completed'] and result['cleanup_complete'], result)
            self.assertEqual(result['exit_code'], 0)
            self.assertFalse(result['timed_out'])
            self.assertTrue(result['stop_feedback_sent'])
            self.assertEqual(result['stop_hook_receipts'], [
                {'request': {'id': 'hook-0', 'sessionId': 'runtime-session',
                             'hookCallbackId': 'benchmark-root-stop', 'hookEventName': 'stop', 'reason': 'end_turn'},
                 'response': {'decision': 'deny', 'systemMessage': feedback}, 'feedback_sent': True},
                {'request': {'id': 'hook-1', 'sessionId': 'runtime-session',
                             'hookCallbackId': 'benchmark-root-stop', 'hookEventName': 'stop', 'reason': 'end_turn'},
                 'response': {'decision': 'continue'}, 'feedback_sent': False},
            ])
            replies = json.loads((root / 'hook-responses.json').read_text())
            self.assertEqual([r['result'] for r in replies],
                             [r['response'] for r in result['stop_hook_receipts']])
            self.assertTrue((root / 'tools-completed').exists())
            self.assertEqual(result, json.loads((root / 'acp-result.json').read_text()))

    def test_unrelated_hooks_do_not_consume_root_stop_feedback(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            feedback = 'Please check whether the requested work is complete.'
            result = drive(root, 'stop-feedback-scope', stop_feedback=feedback)
            self.assertTrue(result['completed'] and result['cleanup_complete'], result)
            self.assertEqual(result['exit_code'], 0)
            self.assertTrue(result['stop_feedback_sent'])
            receipts = result['stop_hook_receipts']
            self.assertEqual(len(receipts), 6)
            self.assertEqual([r['feedback_sent'] for r in receipts], [False, False, False, False, True, False])
            self.assertEqual([r['response'] for r in receipts], [
                {'decision': 'continue'}, {'decision': 'continue'},
                {'decision': 'continue'}, {'decision': 'continue'},
                {'decision': 'deny', 'systemMessage': feedback}, {'decision': 'continue'},
            ])
            replies = json.loads((root / 'hook-responses.json').read_text())
            self.assertEqual([r['result'] for r in replies], [r['response'] for r in receipts])
            self.assertTrue((root / 'tools-completed').exists())

    def test_internal_reload_acks_are_logged_without_completing_prompt_or_pending_children(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = drive(root, 'reload-acks')
            self.assertTrue(result['completed'] and result['cleanup_complete'], result)
            self.assertEqual(result['exit_code'], 0)
            self.assertEqual(result['prompt_receipt']['id'], 'benchmark-5')
            self.assertEqual(result['prompt_receipt']['result'], {'stopReason': 'end_turn'})
            self.assertTrue((root / 'tools-completed').exists())
            received = [row['message'] for row in map(json.loads, (root / 'acp-transcript.jsonl').read_text().splitlines())
                        if row['direction'] == 'received']
            acks = [message for message in received if message.get('id') in ('skills-reload', 'workflows-reload')]
            self.assertEqual(acks, [
                {'jsonrpc': '2.0', 'id': 'skills-reload', 'result': {'result': {'reloaded': 1}}},
                {'jsonrpc': '2.0', 'id': 'workflows-reload', 'result': {'result': {'reloaded': 0}}},
            ])
            self.assertLess(received.index(acks[0]), received.index(result['prompt_receipt']))
            self.assertLess(received.index(result['prompt_receipt']), received.index(acks[1]))
            self.assertTrue(any(message.get('params', {}).get('update', {}).get('sessionUpdate') == 'subagent_finished'
                                for message in received[received.index(acks[1]) + 1:]))

    def test_unknown_malformed_or_failed_reload_and_wrong_prompt_responses_remain_errors(self):
        for scenario in ('reload-unknown', 'reload-malformed', 'reload-rpc-error', 'reload-error', 'prompt-wrong-id'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                result = drive(Path(directory), scenario)
                self.assertFalse(result['completed'])
                self.assertTrue(result['cleanup_complete'], result)
                self.assertFalse(result['timed_out'])
                expected = ('Unexpected response after prompt completion' if scenario == 'reload-unknown' else
                            'Unexpected ACP response ID' if scenario == 'prompt-wrong-id' else
                            'Invalid internal reload acknowledgement')
                self.assertIn(expected, result['error'])

    def test_one_process_snapshot_timeout_recovers_without_aborting_inference(self):
        real_popen = subprocess.Popen
        delayed = False

        def snapshot(command, **kwargs):
            nonlocal delayed
            if command[0] == 'ps' and not delayed:
                delayed = True
                process = mock.Mock()
                process.communicate.side_effect = subprocess.TimeoutExpired(command, 0)
                return process
            return real_popen(command, **kwargs)

        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(acp_driver.subprocess, 'Popen', side_effect=snapshot):
            result = drive(Path(directory))
        self.assertTrue(delayed)
        self.assertTrue(result['completed'] and result['cleanup_complete'], result)
        self.assertFalse(result['timed_out'])

    def test_persistent_snapshot_timeout_cannot_confirm_cleanup_from_stale_state(self):
        tree = acp_driver.OwnedTree(mock.Mock(pid=123))
        tree.live = set()  # Even a previously empty snapshot cannot prove current cleanup.
        ps = mock.Mock()
        ps.communicate.side_effect = subprocess.TimeoutExpired(['ps'], 5)
        with mock.patch.object(acp_driver.subprocess, 'Popen', return_value=ps) as snapshot, \
                mock.patch.object(acp_driver.os, 'kill'), mock.patch.object(acp_driver.os, 'killpg'), \
                mock.patch.object(acp_driver, '_terminate_owned_group'):
            with self.assertRaises(subprocess.TimeoutExpired):
                tree.stop()
        self.assertEqual(snapshot.call_count, 2)
        self.assertEqual(tree.checked, 0)

    def test_cleanup_deadline_bounds_snapshots_waits_and_keeps_known_signals(self):
        process = mock.Mock(pid=123)
        process.poll.return_value = None
        process.wait.side_effect = subprocess.TimeoutExpired('peer', 0)
        tree = acp_driver.OwnedTree(process)
        tree.pids.add(456)
        tree.live = set()  # Stale state must not discard a known descendant.
        deadline = time.monotonic() + .1

        def communicate(**kwargs):
            self.assertGreater(kwargs['timeout'], 0)
            self.assertLessEqual(kwargs['timeout'], .1)
            time.sleep(kwargs['timeout'])
            raise subprocess.TimeoutExpired(['ps'], kwargs['timeout'])

        snapshot = mock.Mock()
        snapshot.communicate.side_effect = communicate
        snapshot.wait.side_effect = subprocess.TimeoutExpired(['ps'], 0)
        with mock.patch.object(acp_driver.subprocess, 'Popen', return_value=snapshot) as ps, \
                mock.patch.object(acp_driver.os, 'kill') as kill, \
                mock.patch.object(acp_driver.os, 'killpg'):
            with self.assertRaises(TimeoutError):
                tree.stop(deadline=deadline)
        self.assertEqual(ps.call_count, 1)
        snapshot.kill.assert_called_once_with()
        snapshot.wait.assert_called_once_with(timeout=0)
        kill.assert_any_call(456, acp_driver.signal.SIGTERM)
        kill.assert_any_call(456, acp_driver.signal.SIGKILL)
        for call in process.wait.call_args_list:
            self.assertIn('timeout', call.kwargs)
            self.assertEqual(call.kwargs['timeout'], 0)

        process.reset_mock()
        with mock.patch.object(runner.os, 'killpg', side_effect=ProcessLookupError):
            runner._terminate_owned_group(process, deadline=deadline)
        process.wait.assert_called_once_with(timeout=0)

    def test_exit_plan_mode_uses_native_headless_approval_and_finishes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = drive(root, 'exit-plan-mode')
            self.assertTrue(result['completed'] and result['cleanup_complete'], result)
            self.assertEqual(result['exit_code'], 0)
            self.assertEqual(json.loads((root / 'plan-response.json').read_text())['result'],
                             {'outcome': 'approved'})
            self.assertTrue((root / 'tools-completed').exists())

    def test_off_and_auto_are_explicitly_acknowledged(self):
        with tempfile.TemporaryDirectory() as directory:
            result = drive(Path(directory), ultracode=False, effort=None)
            self.assertTrue(result['completed'], result)
            self.assertEqual(result['activation_receipt']['result'], {'enabled': False})
            self.assertIs(result['model_receipt']['result']['_meta']['reasoningEffortAuto'], True)

    def test_real_order_model_effort_activation_permission_and_completion_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = drive(root)
            self.assertTrue(result['completed'], result)
            self.assertEqual(result['exit_code'], 0)
            self.assertEqual(result['session'], 'runtime-session')
            self.assertEqual(result['activation_receipt']['result'], {'enabled': True})
            transcript = [json.loads(line) for line in (root / 'acp-transcript.jsonl').read_text().splitlines()]
            requests = [row['message'] for row in transcript if row['direction'] == 'sent' and 'method' in row['message']]
            self.assertEqual([r['method'] for r in requests], [
                'initialize', 'session/new', 'session/set_model', acp_driver.ULTRACODE_METHOD, 'session/prompt'])
            self.assertEqual(requests[1]['params']['_meta'], {'sessionKind': 'headless'})
            self.assertNotIn('stop_hook_receipts', result)
            self.assertNotIn('stop_feedback_sent', result)
            self.assertEqual(requests[2]['params'], {'sessionId': 'runtime-session',
                'modelId': 'chatgpt/gpt-6.1-sol', '_meta': {'reasoningEffort': 'medium', 'reasoningEffortAuto': False}})
            self.assertTrue((root / 'tools-completed').exists())
            reply = json.loads((root / 'permission-response.json').read_text())
            self.assertEqual(reply['result'], {'outcome': {'outcome': 'selected', 'optionId': 'yes-once'}})
            self.assertEqual(result, json.loads((root / 'acp-result.json').read_text()))

    def test_unconfirmed_activation_or_model_never_sends_prompt(self):
        for scenario in ('activation-false', 'activation-missing', 'activation-null', 'activation-error', 'wrong-model', 'wrong-effort'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result = drive(root, scenario)
                self.assertFalse(result['completed'])
                self.assertTrue(result['error'] and result['cleanup_complete'], result)
                self.assertFalse((root / 'prompt-seen').exists())
                self.assertFalse(result['setup_confirmed'])

    def test_missing_or_negative_stop_and_unsupported_permissions_cannot_succeed(self):
        for scenario in ('missing-stop', 'negative-stop', 'outside-permission', 'unsupported-request'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result = drive(root, scenario)
                self.assertFalse(result['completed'])
                self.assertTrue(result['error'] and result['cleanup_complete'], result)
                self.assertTrue(result['setup_confirmed'])
                if scenario == 'outside-permission' and (root / 'permission-response.json').exists():
                    self.assertEqual(json.loads((root / 'permission-response.json').read_text())['result'],
                                     {'outcome': {'outcome': 'cancelled'}})

    def test_timeout_and_eof_stop_owned_writer_before_return(self):
        for scenario in ('timeout', 'eof', 'stubborn'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                started = time.monotonic()
                timeout = 3 if scenario == 'eof' else .9
                result = drive(root, scenario, timeout=timeout)
                self.assertLess(time.monotonic() - started, timeout + .1)
                self.assertFalse(result['completed'])
                self.assertTrue(result['cleanup_complete'], result)
                self.assertEqual(result['timed_out'], scenario != 'eof')
                self.assertFalse((root / 'prompt-seen').exists())
                pulse = root / 'pulse'
                self.assertTrue(pulse.exists(), (root / 'stderr.txt').read_text())
                last = pulse.read_text()
                time.sleep(.06)
                self.assertEqual(pulse.read_text(), last, 'owned writer survived cleanup')


if __name__ == '__main__':
    unittest.main()
