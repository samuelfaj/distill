def paginate(rows, page, size):
    if page < 1:
        raise ValueError('page starts at 1')
    start = (page - 1) * size
    return rows[start:start + size]
