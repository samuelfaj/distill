import unittest

from bus import EventBus


class BusTests(unittest.TestCase):
    def test_once(self):
        bus, seen = EventBus(), []
        bus.subscribe('a', seen.append, once=True)
        bus.emit('a', 1)
        bus.emit('a', 2)
        self.assertEqual(seen, [1])

    def test_error_does_not_stop_other_handlers(self):
        bus, seen = EventBus(), []

        def broken(payload):
            raise RuntimeError('boom')
        bus.subscribe('a', broken)
        bus.subscribe('a', seen.append)
        with self.assertRaises(RuntimeError):
            bus.emit('a', 1)
        self.assertEqual(seen, [1])

    def test_unsubscribe_while_delivering(self):
        bus, seen = EventBus(), []
        stop = None

        def first(payload):
            seen.append(('first', payload))
            stop()
        stop = bus.subscribe('a', first)
        bus.subscribe('a', lambda payload: seen.append(('second', payload)))
        bus.emit('a', 1)
        bus.emit('a', 2)
        self.assertEqual(seen, [('first', 1), ('second', 1), ('second', 2)])
