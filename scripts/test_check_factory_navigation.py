import unittest
from check_factory_navigation import factory_click, FactoryDaemon, GRAPH
from lifecycle_lib import DockScreen, wire2_decode


class FactoryNavigationGuards(unittest.TestCase):
    def test_click_targets_the_observed_factory_cells(self):
        screen = DockScreen()
        screen.cells[3][5:12] = list('factory')
        self.assertEqual(factory_click(screen), '\x1b[<0;7;4M\x1b[<0;7;4m')

    def test_existing_factory_response_is_replaced_without_suppressing_errors(self):
        server = FactoryDaemon.__new__(FactoryDaemon)
        sent = []
        sock = type('Socket', (), {'sendall': lambda _self, value: sent.append(wire2_decode(value.hex()))})()
        server.send(sock, {'type': 'response', 'id': 'graph', 'command': 'factory_activity',
                           'success': True, 'data': {'activity': [], 'jobs': []}})
        self.assertEqual(sent, [{'type': 'reply', 'id': 'graph', 'status': 'ok', 'result': GRAPH}])

    def test_missing_factory_is_not_silently_clicked(self):
        with self.assertRaises(AssertionError):
            factory_click(DockScreen())


if __name__ == '__main__':
    unittest.main()
