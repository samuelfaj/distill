#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        bus_module, = import_fresh(fixture_dir(argv, 'hard_event_ordering'), 'bus')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    EventBus = bus_module.EventBus

    def nested_order():
        bus, trace = EventBus(), []
        bus.subscribe('a', lambda p: (trace.append('a1'), bus.emit('b', p)))
        bus.subscribe('a', lambda p: trace.append('a2'))
        bus.subscribe('b', lambda p: (trace.append('b1'), bus.emit('c', p)))
        bus.subscribe('c', lambda p: trace.append('c1'))
        bus.subscribe('b', lambda p: trace.append('b2'))
        bus.emit('a', 0)
        return trace
    attempt(failures, 'nested events wait for the current one', nested_order, ['a1', 'a2', 'b1', 'b2', 'c1'])

    def snapshot():
        bus, trace = EventBus(), []
        late = lambda p: trace.append(('late', p))
        bus.subscribe('a', lambda p: (trace.append(('first', p)), p == 1 and bus.subscribe('a', late)))
        bus.emit('a', 1)
        bus.emit('a', 2)
        return trace
    attempt(failures, 'handlers added during delivery wait for the next event', snapshot,
            [('first', 1), ('first', 2), ('late', 2)])

    def once_reentrant():
        bus, trace, count = EventBus(), [], [0]

        def again(p):
            trace.append(('again', p))
            if count[0] == 0:
                count[0] += 1
                bus.emit('a', 'second')
        bus.subscribe('a', again)
        bus.subscribe('a', lambda p: trace.append(('once', p)), once=True)
        bus.emit('a', 'first')
        bus.emit('a', 'third')
        return trace
    attempt(failures, 'once handler runs for one event even when re-emitted', once_reentrant,
            [('again', 'first'), ('once', 'first'), ('again', 'second'), ('again', 'third')])

    def first_error_after_queue():
        bus, trace = EventBus(), []

        def boom(p):
            trace.append('boom')
            bus.emit('b', p)
            raise KeyError('first')

        def later(p):
            trace.append('later')
            raise ValueError('second')
        bus.subscribe('a', boom)
        bus.subscribe('a', lambda p: trace.append('a2'))
        bus.subscribe('b', later)
        bus.subscribe('b', lambda p: trace.append('b2'))
        try:
            bus.emit('a', 0)
        except Exception as error:
            return trace, type(error).__name__
        return trace, None
    attempt(failures, 'first error raised after every queued event', first_error_after_queue,
            (['boom', 'a2', 'later', 'b2'], 'KeyError'))

    def usable_after_error():
        bus, trace = EventBus(), []

        def fails_once(p):
            if p == 1:
                bus.emit('b', 'queued')
                raise RuntimeError('x')
            trace.append(p)
        bus.subscribe('a', fails_once)
        bus.subscribe('b', trace.append)
        try:
            bus.emit('a', 1)
        except RuntimeError:
            pass
        bus.emit('a', 2)
        bus.emit('b', 3)
        return trace
    attempt(failures, 'bus works normally after an error', usable_after_error, ['queued', 2, 3])

    def unsubscribe_once_before_it_runs():
        bus, trace = EventBus(), []
        stop = bus.subscribe('a', trace.append, once=True)
        stop()
        bus.emit('a', 1)
        stop()
        return trace
    attempt(failures, 'unsubscribing a once handler', unsubscribe_once_before_it_runs, [])

    def twice():
        bus, trace = EventBus(), []
        bus.subscribe('a', trace.append)
        bus.subscribe('a', trace.append)
        bus.emit('a', 1)
        return trace
    attempt(failures, 'a handler subscribed twice runs twice', twice, [1, 1])
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
