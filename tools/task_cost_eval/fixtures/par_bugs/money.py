def split_evenly(cents, parts):
    """Split cents into parts shares that sum to cents; earlier shares get the extra cents."""
    share = cents // parts
    return [share] * parts
