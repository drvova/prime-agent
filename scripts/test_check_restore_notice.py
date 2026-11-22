#!/usr/bin/env python3
"""Guard that the headless skill availability is read from the model-visible inventory."""
import copy
import unittest

from check_restore_notice import skill_availability


HIDDEN = {'agent-message': True, 'agent-observe': False, 'rlm-heartbeat': False}


class AvailabilityTests(unittest.TestCase):
    def test_observation_uses_actual_tool_result_and_inventory(self):
        requests = [
            {'messages': [{'role': 'system', 'content': '<available_skills><skill><name>agent-message</name></skill></available_skills>'}]}]
        self.assertEqual(skill_availability(requests), HIDDEN)
        broken = copy.deepcopy(requests)
        broken[0]['messages'][0]['content'] = 'agent-message'
        with self.assertRaises(AssertionError):
            skill_availability(broken)


if __name__ == '__main__':
    unittest.main()
