class FeedError(ValueError):
    def __init__(self, code, row, field):
        self.code, self.row, self.field = code, row, field
        super().__init__(f'{code} at row {row}: {field}')
