class Ledger:
    def __init__(self, initial):
        if any(not isinstance(name,str) or type(amount) is not int or amount < 0 for name,amount in initial.items()):
            raise ValueError('invalid initial balance')
        self.balances = dict(initial)
        self.last_seq = 0
        self.seen = {}
        self.reversed = set()
        self.journal = []
