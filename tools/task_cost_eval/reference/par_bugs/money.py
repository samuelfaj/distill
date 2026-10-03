def split_evenly(cents, parts):
    share, extra = divmod(cents, parts)
    return [share + (1 if index < extra else 0) for index in range(parts)]
