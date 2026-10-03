def to_fahrenheit(celsius):
    if isinstance(celsius, bool) or not isinstance(celsius, (int, float)):
        raise TypeError("celsius must be a number")
    if celsius < -273.15:
        raise ValueError("below absolute zero")
    return celsius * 9 / 5 + 32
