Implement the two small query-string utilities in tools/task_cost_eval/fixtures/ultra_heldout_querycontrol/query.py with Python's standard library. Edit the file and retain the signatures.

encode_query(pairs): pairs is an iterable of (key,value) where key is str and value is str or None. Reject invalid key/value types with ValueError, including a non-string key when its value is None. Omit pairs whose value is None; preserve duplicate remaining pairs and empty keys/values. Sort by unencoded key then unencoded value (ordinary Python string order), percent-encode UTF-8 keys/values using only ASCII letters/digits and -._~ as safe, join key=value pairs with '&'. Spaces are %20, never '+', slash and '+' are encoded. Empty result is ''. Do not mutate the input; accept generators.

redact_pairs(pairs,sensitive): pairs has the same valid shape; sensitive is an iterable of strings. Return a new list in original order, replacing each matching value (including None) with '[REDACTED]'. Match whole keys using Unicode casefold; do not redact by substring. Preserve original key spelling and nonmatching values, duplicates included. Never mutate inputs, accept generators.

No CLI or dependencies are needed for these pure functions. Run PYTHONDONTWRITEBYTECODE=1 python -m unittest discover -v from this slice.
