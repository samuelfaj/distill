import re


def words(text):
    return re.findall(r'[^\W_]+', text.casefold())
