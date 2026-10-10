from state import Ledger
from projector import apply
from accounts import snapshot


def replay(initial, events):
    ledger = Ledger(initial)
    for event in events:
        apply(ledger,event)
    return snapshot(ledger)
