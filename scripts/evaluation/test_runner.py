import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import accepted_report, process


class RunnerContracts(unittest.TestCase):
    def test_missing_duplicate_and_skipped_requirements_never_pass(self):
        self.assertFalse(accepted_report({}, ['state']))
        self.assertFalse(accepted_report(None, ['state']))
        self.assertFalse(accepted_report({'requirements': [None]}, ['state']))
        self.assertFalse(accepted_report({'requirements': [{'id': 'state', 'status': 'passed'}]}, ['state', 'route']))
        self.assertFalse(accepted_report({'requirements': [{'id': 'state', 'status': 'passed'}] * 2}, ['state']))
        self.assertFalse(accepted_report({'requirements': [{'id': 'state', 'status': 'skipped'}]}, ['state']))
        self.assertTrue(accepted_report({'requirements': [{'id': 'state', 'status': 'passed'}]}, ['state']))

    @unittest.skipUnless(os.name == 'posix', 'process groups require POSIX')
    def test_timeout_stops_delayed_candidate_mutation_and_keeps_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            code = 'import time; print("started", flush=True); time.sleep(2); open("late", "w").write("changed")'
            value = process([sys.executable, '-c', code], root, dict(os.environ), .15, root / 'child')
            self.assertEqual(value['status'], 'timeout')
            self.assertFalse(value['process_group_remaining'])
            self.assertEqual((root / 'child.stdout').read_text().strip(), 'started')
            self.assertFalse((root / 'late').exists())


if __name__ == '__main__':
    unittest.main()
