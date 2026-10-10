import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from . import acp_driver


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
        if scenario == 'missing-stop':
            sys.exit(0)
        result = {'stopReason': 'refusal' if scenario == 'negative-stop' else 'end_turn'}
        send({'id': request['id'], 'result': result})
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


def drive(root, scenario='success', timeout=3, ultracode=True, effort='medium'):
    peer = fake_peer(root, scenario)
    return acp_driver.run_acp([str(peer)], cwd=root, environment=os.environ.copy(), prompt='Change fixture',
                             model='chatgpt/gpt-6.1-sol', effort=effort, ultracode=ultracode,
                             timeout=timeout, output=root)


class AcpDriverTest(unittest.TestCase):
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
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory, \
                    mock.patch('tools.task_cost_eval.runner.TERMINATE_GRACE_SECONDS', .1):
                root = Path(directory)
                started = time.monotonic()
                result = drive(root, scenario, timeout=3 if scenario == 'eof' else .7)
                self.assertLess(time.monotonic() - started, 4)
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
