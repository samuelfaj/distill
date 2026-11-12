def clamp(value, low, high):
    """Limit value to the range [low, high]."""
    if value < low:
        return low
    if value > high:
        return value
    return value
