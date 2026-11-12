#!/usr/bin/env python3
"""Deterministic generator for the long-v1 cohort: fixtures, reference solutions and grader data.

    python3 -B -m tools.task_cost_eval.generate_long_fixtures [--root DIR]

Each task spreads one kind of change over 40 to 50 files with a per-file check, so
finishing it takes many tool rounds. The committed files are this script's
output; test_mode_cohorts regenerates them into a scratch root and compares
bytes, so edit this script, rerun it, and commit both. Expected grader values
come from executing the generated code (old behaviour for the migrations, the
reference for the cleaning task), never from hand-written tables. The JavaScript
task needs Node.js to compute its expectations.
"""
import argparse
import json
import random
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TASKS = ('long_handler_migration', 'long_clean_pipeline', 'long_money_rename', 'long_cjs_to_esm')
NOUNS = ('orders', 'invoices', 'refunds', 'carts', 'coupons', 'shipments', 'returns', 'vendors', 'payouts',
         'ledgers', 'budgets', 'quotes', 'receipts', 'tickets', 'alerts', 'reports', 'sessions', 'tokens', 'teams',
         'members', 'projects', 'tasks', 'comments', 'labels', 'files', 'folders', 'uploads', 'exports', 'imports',
         'schedules', 'reminders', 'events', 'venues', 'bookings', 'rooms', 'guests', 'menus', 'dishes', 'stocks',
         'batches', 'routes', 'drivers', 'vehicles', 'zones', 'plans', 'trials', 'metrics', 'surveys', 'answers',
         'badges')


def d(text):
    return textwrap.dedent(text).lstrip('\n')


class Tree:
    """Files of one task: fixture, reference (fixture plus overrides) and grader data."""

    def __init__(self, name):
        self.name, self.fixture, self.reference, self.expected = name, {}, {}, None

    def both(self, path, text):
        self.fixture[path] = self.reference[path] = text

    def write(self, root):
        for kind, files in (('fixtures', self.fixture), ('reference', self.reference)):
            base = root / kind / self.name
            if base.exists():
                shutil.rmtree(base)
            for path, text in sorted(files.items()):
                target = base / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(text)
        data = root / 'graders' / f'{self.name}_expected.json'
        data.parent.mkdir(parents=True, exist_ok=True)
        data.write_text(json.dumps(self.expected, indent=1, sort_keys=True) + '\n')


def run_python(directory, code):
    result = subprocess.run([sys.executable, '-B', '-c', code], cwd=directory, capture_output=True, text=True,
                            timeout=120, env={'PATH': '/usr/bin:/bin', 'PYTHONDONTWRITEBYTECODE': '1'})
    if result.returncode:
        raise RuntimeError(result.stderr)
    return json.loads(result.stdout)


def materialize(tree, kind, scratch):
    base = scratch / kind / tree.name
    for path, text in (tree.fixture if kind == 'fixtures' else tree.reference).items():
        (base / path).parent.mkdir(parents=True, exist_ok=True)
        (base / path).write_text(text)
    return base


# --------------------------------------------------------------------------- handler migration

FRAMEWORK_OLD = d('''
    """Tiny request router.

    register() is the old API: its handlers return a dict with status, body and
    optional headers, or a (status, body) tuple. route() and Response are the
    new API: handlers decorated with route() return a Response.
    """
    _ROUTES = {}


    class Response:
        def __init__(self, status, body, headers=None):
            self.status = status
            self.body = body
            self.headers = dict(headers or {})

        def __eq__(self, other):
            return isinstance(other, Response) and (self.status, self.body, self.headers) == (
                other.status, other.body, other.headers)

        def __repr__(self):
            return f'Response({self.status!r}, {self.body!r}, {self.headers!r})'


    def _add(name, handler, methods, legacy):
        if name in _ROUTES:
            raise ValueError(f'route {name} registered twice')
        _ROUTES[name] = (handler, tuple(methods), legacy)


    def register(name, handler, methods=('GET',)):
        _add(name, handler, methods, legacy=True)


    def route(name, methods=('GET',)):
        def decorate(handler):
            _add(name, handler, methods, legacy=False)
            return handler
        return decorate


    def _from_legacy(result):
        if isinstance(result, tuple):
            status, body = result
            return Response(status, body)
        return Response(result['status'], result['body'], result.get('headers'))


    def routes():
        return {name: list(methods) for name, (_, methods, _) in sorted(_ROUTES.items())}


    def handlers_in(module):
        return {name: handler for name, (handler, _, _) in _ROUTES.items() if handler.__module__ == module}


    def dispatch(name, method, request):
        handler, methods, legacy = _ROUTES[name]
        if method not in methods:
            return Response(405, {'error': 'method not allowed'})
        result = handler(request)
        if legacy:
            return _from_legacy(result)
        if not isinstance(result, Response):
            raise TypeError(f'{name} returned {type(result).__name__}, not Response')
        return result
''')

FRAMEWORK_NEW = d('''
    """Tiny request router: handlers decorated with route() return a Response."""
    _ROUTES = {}


    class Response:
        def __init__(self, status, body, headers=None):
            self.status = status
            self.body = body
            self.headers = dict(headers or {})

        def __eq__(self, other):
            return isinstance(other, Response) and (self.status, self.body, self.headers) == (
                other.status, other.body, other.headers)

        def __repr__(self):
            return f'Response({self.status!r}, {self.body!r}, {self.headers!r})'


    def route(name, methods=('GET',)):
        def decorate(handler):
            if name in _ROUTES:
                raise ValueError(f'route {name} registered twice')
            _ROUTES[name] = (handler, tuple(methods))
            return handler
        return decorate


    def routes():
        return {name: list(methods) for name, (_, methods) in sorted(_ROUTES.items())}


    def handlers_in(module):
        return {name: handler for name, (handler, _) in _ROUTES.items() if handler.__module__ == module}


    def dispatch(name, method, request):
        handler, methods = _ROUTES[name]
        if method not in methods:
            return Response(405, {'error': 'method not allowed'})
        result = handler(request)
        if not isinstance(result, Response):
            raise TypeError(f'{name} returned {type(result).__name__}, not Response')
        return result
''')

HANDLER_APP = d('''
    import importlib
    import pkgutil
    import sys
    from pathlib import Path

    HERE = Path(__file__).resolve().parent


    def load_all():
        """Import every handler module once; each registers its routes at import time."""
        if str(HERE) not in sys.path:
            sys.path.insert(0, str(HERE))
        for module in pkgutil.iter_modules([str(HERE / 'handlers')]):
            importlib.import_module(f'handlers.{module.name}')
''')

HANDLER_CHECK = d('''
    """Check one handler module: python3 check.py handlers/NAME.py"""
    import importlib
    import sys
    from pathlib import Path

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import framework  # noqa: E402


    def main(argv):
        if len(argv) != 2:
            print('usage: python3 check.py handlers/NAME.py', file=sys.stderr)
            return 2
        module = f'handlers.{Path(argv[1]).stem}'
        importlib.import_module(module)
        handlers = framework.handlers_in(module)
        if not handlers:
            print(f'{module}: no routes registered with route()', file=sys.stderr)
            return 1
        failed = False
        for name, handler in sorted(handlers.items()):
            result = handler({'id': 1, 'qty': 2})
            if not isinstance(result, framework.Response):
                print(f'{name}: returned {type(result).__name__}, expected Response', file=sys.stderr)
                failed = True
        if not failed:
            print(f'ok {module}: {", ".join(sorted(handlers))}')
        return 1 if failed else 0


    if __name__ == '__main__':
        raise SystemExit(main(sys.argv))
''')


def handler_module(index, noun, new):
    rng = random.Random(f'handler-{index}')
    unit = rng.randint(2, 40)
    shape, extra, guarded = index % 4, (index // 4) % 3, index % 3 == 0
    lines = [f'from framework import {"Response, route" if new else "register"}', '', f'UNIT = {unit}', '', '']

    def legacy(status, body, headers=None):
        if shape == 1 and headers is None:
            return f'return {status}, {body}'
        extra_headers = f", 'headers': {headers}" if headers else ''
        return f"return {{'status': {status}, 'body': {body}{extra_headers}}}"

    def modern(status, body, headers=None):
        return f'return Response({status}, {body}{", " + headers if headers else ""})'

    ret = modern if new else legacy
    if shape == 3:
        lines += ['def _ok(body):', f'    {ret(200, "body")}', '', '']
    if new:
        lines.append(f"@route('{noun}.get')")
        if extra == 1:
            lines.append(f"@route('{noun}.fetch')")
    lines.append('def _get(request):')
    if guarded:
        lines += ["    if 'id' not in request:", f"        {ret(404, repr({'error': 'missing id'}))}"]
    lines += ["    item_id = request.get('id', 0)",
              f"    body = {{'id': item_id, 'kind': '{noun}', 'total': request.get('qty', 1) * UNIT}}"]
    if shape == 3:
        lines.append('    return _ok(body)')
    elif shape == 2:
        lines.append(f"    {ret(200, 'body', repr({'Cache-Control': f'max-age={unit}'}))}")
    else:
        lines.append(f"    {ret(200, 'body')}")
    if extra == 2:
        location = "{'Location': f'/" + noun + "/{item_id}'}"
        created = "{'id': item_id, 'created': True}"
        lines += ['', '']
        if new:
            lines.append(f"@route('{noun}.create', methods=('POST',))")
        lines += ['def _create(request):', "    item_id = request.get('id', 0)",
                  f'    return Response(201, {created}, {location})' if new else
                  f"    return {{'status': 201, 'body': {created}, 'headers': {location}}}"]
    if not new:
        lines += ['', '', f"register('{noun}.get', _get)"]
        if extra == 1:
            lines.append(f"register('{noun}.fetch', _get)")
        if extra == 2:
            lines.append(f"register('{noun}.create', _create, methods=('POST',))")
    return '\n'.join(lines) + '\n'


def handler_migration():
    tree = Tree('long_handler_migration')
    tree.fixture['framework.py'] = FRAMEWORK_OLD
    tree.reference['framework.py'] = FRAMEWORK_NEW
    tree.both('app.py', HANDLER_APP)
    tree.both('check.py', HANDLER_CHECK)
    tree.both('handlers/__init__.py', '')
    tree.both('README.md', d('''
        Request handlers for the internal API. `app.load_all()` imports every module in handlers/,
        and `framework.dispatch(name, method, request)` answers a request.

        Check one handler module with `python3 check.py handlers/NAME.py`.
    '''))
    for index, noun in enumerate(NOUNS[:48]):
        tree.fixture[f'handlers/{noun}.py'] = handler_module(index, noun, new=False)
        tree.reference[f'handlers/{noun}.py'] = handler_module(index, noun, new=True)
    return tree


HANDLER_EXPECTED = d('''
    import json
    import app
    import framework
    app.load_all()
    calls = []
    for name, methods in framework.routes().items():
        for method in (*methods, 'DELETE'):
            for request in ({'id': 7, 'qty': 3}, {'id': 2}, {}):
                r = framework.dispatch(name, method, dict(request))
                calls.append({'route': name, 'method': method, 'request': request,
                              'response': {'status': r.status, 'body': r.body, 'headers': r.headers}})
    print(json.dumps({'routes': framework.routes(), 'calls': calls}))
''')

# --------------------------------------------------------------------------- cleaning pipeline

NORMALIZERS = {
    'phone': {
        'code': d(r'''
            import re

            COUNTRY_CODE = '@CC@'
            LENGTH = @LENGTH@


            def normalize(value):
                digits = re.sub(r'\D', '', value)
                if digits.startswith('00' + COUNTRY_CODE):
                    digits = digits[2:]
                if len(digits) == LENGTH:
                    digits = COUNTRY_CODE + digits
                if len(digits) != len(COUNTRY_CODE) + LENGTH or not digits.startswith(COUNTRY_CODE):
                    return None
                return '+' + digits
        '''),
        'defects': [(r"re.sub(r'\D', '', value)", "value.replace(' ', '')"),
                    ('digits = digits[2:]', 'digits = digits[1:]'),
                    ('if len(digits) == LENGTH:', 'if len(digits) >= LENGTH:')],
        'spec': ('Phone number. Ignore every character that is not a digit. A leading international prefix 00@CC@ '
                 'counts as the country code @CC@. Exactly @LENGTH@ digits are a national number and get country '
                 'code @CC@ in front; @CC@ followed by @LENGTH@ digits is already complete. Output + and the '
                 'complete number; anything else is invalid.'),
        'params': [{'CC': '55', 'LENGTH': '11'}, {'CC': '44', 'LENGTH': '10'}, {'CC': '1', 'LENGTH': '10'},
                   {'CC': '351', 'LENGTH': '9'}, {'CC': '49', 'LENGTH': '11'}],
    },
    'date': {
        'code': d(r'''
            import datetime
            import re

            ORDER = '@ORDER@'


            def normalize(value):
                value = value.strip()
                match = re.fullmatch(r'(\d{4})-(\d{2})-(\d{2})', value)
                if match:
                    year, month, day = map(int, match.groups())
                else:
                    match = re.fullmatch(r'(\d{1,2})/(\d{1,2})/(\d{4})', value)
                    if not match:
                        return None
                    first, second, year = map(int, match.groups())
                    day, month = (first, second) if ORDER == 'dmy' else (second, first)
                try:
                    return datetime.date(year, month, day).isoformat()
                except ValueError:
                    return None
        '''),
        'defects': [("(first, second) if ORDER == 'dmy' else (second, first)",
                     "(second, first) if ORDER == 'dmy' else (first, second)"),
                    ('value = value.strip()', 'value = value'),
                    ('    try:\n        return datetime.date(year, month, day).isoformat()\n'
                     '    except ValueError:\n        return None', "    return f'{year:04}-{month:02}-{day:02}'")],
        'spec': ('Date. Surrounding spaces are ignored. Accept YYYY-MM-DD, or D/M/YYYY written in @WORDS@ order '
                 '(one or two digits for day and month). Output YYYY-MM-DD; dates that do not exist, such as '
                 'February 30, are invalid.'),
        'params': [{'ORDER': 'dmy', 'WORDS': 'day/month/year'}, {'ORDER': 'mdy', 'WORDS': 'month/day/year'},
                   {'ORDER': 'dmy', 'WORDS': 'day/month/year'}, {'ORDER': 'mdy', 'WORDS': 'month/day/year'},
                   {'ORDER': 'dmy', 'WORDS': 'day/month/year'}],
    },
    'amount': {
        'code': d(r'''
            from decimal import ROUND_HALF_UP, Decimal, InvalidOperation

            PLACES = Decimal('@QUANT@')


            def normalize(value):
                text = value.strip().replace(',', '')
                negative = text.startswith('(') and text.endswith(')')
                if negative:
                    text = text[1:-1]
                try:
                    amount = Decimal(text)
                except InvalidOperation:
                    return None
                if not amount.is_finite():
                    return None
                amount = amount.quantize(PLACES, rounding=ROUND_HALF_UP)
                return str(-amount if negative else amount)
        '''),
        'defects': [(".replace(',', '')", ''), ('rounding=ROUND_HALF_UP', "rounding='ROUND_HALF_EVEN'"),
                    ('return str(-amount if negative else amount)', 'return str(amount)')],
        'spec': ('Amount. Commas are thousands separators. An amount in parentheses, like (12.50), is negative. '
                 'Round half up to @QUANT@ and output a plain decimal string with exactly that many places. '
                 'Text that is not a finite number is invalid.'),
        'params': [{'QUANT': '0.01'}, {'QUANT': '1'}, {'QUANT': '0.001'}, {'QUANT': '0.01'}, {'QUANT': '0.1'}],
    },
    'email': {
        'code': d(r'''
            REQUIRED_SUFFIX = '@SUFFIX@'


            def normalize(value):
                value = value.strip()
                if value.count('@') != 1 or ' ' in value:
                    return None
                local, domain = value.split('@')
                if not local or '.' not in domain.strip('.'):
                    return None
                if REQUIRED_SUFFIX and not domain.lower().endswith(REQUIRED_SUFFIX):
                    return None
                return local + '@' + domain.lower()
        '''),
        'defects': [("return local + '@' + domain.lower()", 'return value.lower()'),
                    ("value.count('@') != 1 or ' ' in value", "'@' not in value"),
                    ('if REQUIRED_SUFFIX and not', 'if False and not')],
        'spec': ('Email. Surrounding spaces are ignored. Exactly one @, no inner spaces, a nonempty local part and '
                 'a domain containing a dot. @SUFFIXTEXT@Output the local part unchanged and the domain in '
                 'lowercase.'),
        'params': [{'SUFFIX': '', 'SUFFIXTEXT': ''}, {'SUFFIX': '.org', 'SUFFIXTEXT': 'The domain must end in .org. '},
                   {'SUFFIX': '.edu', 'SUFFIXTEXT': 'The domain must end in .edu. '}, {'SUFFIX': '', 'SUFFIXTEXT': ''},
                   {'SUFFIX': '.net', 'SUFFIXTEXT': 'The domain must end in .net. '}],
    },
    'country': {
        'code': d(r'''
            ALIASES = @ALIASES@


            def normalize(value):
                key = ' '.join(value.split()).casefold()
                if len(key) == 2 and key.isalpha() and key.upper() in ALIASES.values():
                    return key.upper()
                return ALIASES.get(key)
        '''),
        'defects': [('.casefold()', ''), ('return ALIASES.get(key)', 'return ALIASES.get(key, value)'),
                    ("' '.join(value.split())", 'value.strip()')],
        'spec': ('Country. Case and repeated spaces do not matter. Accept a two-letter code from the table or one '
                 'of these names: @NAMES@. Output the two-letter code in uppercase; anything else is invalid.'),
        'params': [
            {'ALIASES': {'brazil': 'BR', 'brasil': 'BR', 'portugal': 'PT', 'united kingdom': 'GB'}},
            {'ALIASES': {'germany': 'DE', 'deutschland': 'DE', 'france': 'FR', 'new zealand': 'NZ'}},
            {'ALIASES': {'japan': 'JP', 'south africa': 'ZA', 'canada': 'CA'}},
            {'ALIASES': {'mexico': 'MX', 'chile': 'CL', 'costa rica': 'CR', 'peru': 'PE'}},
            {'ALIASES': {'spain': 'ES', 'espana': 'ES', 'italy': 'IT', 'sri lanka': 'LK'}}],
    },
    'postcode': {
        'code': d(r'''
            import re

            PATTERN = re.compile(r'@PATTERN@')


            def normalize(value):
                code = re.sub(r'\s+', '', value).upper()
                return code if PATTERN.fullmatch(code) else None
        '''),
        'defects': [(r"re.sub(r'\s+', '', value)", 'value.strip()'), ('.upper()', ''),
                    ('PATTERN.fullmatch(code)', 'PATTERN.match(code)')],
        'spec': ('Postcode. Remove all whitespace and uppercase letters, then the whole code must match the '
                 'regular expression @PATTERN@; anything else is invalid.'),
        'params': [{'PATTERN': r'[A-Z]\d[A-Z]\d[A-Z]\d'}, {'PATTERN': r'\d{4}[A-Z]{2}'},
                   {'PATTERN': r'[A-Z]{1,2}\d[A-Z\d]?\d[A-Z]{2}'}, {'PATTERN': r'[A-Z]\d{4}[A-Z]{3}'},
                   {'PATTERN': r'\d{3}[A-Z]{2}\d'}],
    },
    'name': {
        'code': d(r'''
            PARTICLES = @PARTICLES@


            def _word(word, first):
                if not first and word.casefold() in PARTICLES:
                    return word.casefold()
                return '-'.join(part[:1].upper() + part[1:].lower() for part in word.split('-'))


            def normalize(value):
                words = value.split()
                if not words:
                    return None
                return ' '.join(_word(word, index == 0) for index, word in enumerate(words))
        '''),
        'defects': [("'-'.join(part[:1].upper() + part[1:].lower() for part in word.split('-'))",
                     'word.capitalize()'),
                    ('if not first and word.casefold() in PARTICLES', 'if word.casefold() in PARTICLES'),
                    ('words = value.split()', "words = value.split(' ')")],
        'spec': ('Person name. Words are separated by any run of whitespace. Each word, and each part of a '
                 'hyphenated word, starts with an uppercase letter and continues in lowercase, except these '
                 'particles, which are lowercase unless they are the first word: @WORDS@. A blank name is invalid.'),
        'params': [{'PARTICLES': ('da', 'de', 'do', 'dos')}, {'PARTICLES': ('van', 'der', 'den')},
                   {'PARTICLES': ('von', 'zu')}, {'PARTICLES': ('de', 'la', 'del')}, {'PARTICLES': ('di', 'da')}],
    },
    'flag': {
        'code': d(r'''
            TRUE = @TRUE@
            FALSE = @FALSE@


            def normalize(value):
                word = value.strip().casefold()
                if word in TRUE:
                    return 'true'
                if word in FALSE:
                    return 'false'
                return None
        '''),
        'defects': [('.casefold()', ''), ("    return None", "    return 'false'"), ('value.strip()', 'value')],
        'spec': ("Yes/no flag. Case and surrounding spaces do not matter. These words mean true: @TRUEWORDS@; these "
                 "mean false: @FALSEWORDS@. Output 'true' or 'false'; any other word is invalid."),
        'params': [{'TRUE': ('yes', 'y', 'true', '1'), 'FALSE': ('no', 'n', 'false', '0')},
                   {'TRUE': ('sim', 's'), 'FALSE': ('nao', 'n')},
                   {'TRUE': ('on', 'enabled'), 'FALSE': ('off', 'disabled')},
                   {'TRUE': ('ja', 'j'), 'FALSE': ('nein', 'n')},
                   {'TRUE': ('active', 'a'), 'FALSE': ('inactive', 'i')}],
    },
}


def normalizer_inputs(kind, params):
    if kind == 'phone':
        cc, length = params['CC'], int(params['LENGTH'])
        number = ''.join(str((3 * i + 7) % 10) for i in range(length))
        return [f'+{cc} {number}', f'00{cc}{number}', number, f'({number[:2]}) {number[2:6]}-{number[6:]}',
                number[:-1], f'{cc}{number}', 'no phone', f'+{cc}-{number[:3]}-{number[3:]}']
    if kind == 'date':
        return ['2026-03-04', ' 2026-12-31 ', '4/3/2026', '13/1/2026', '1/13/2026', '30/2/2024', '2/30/2024',
                '29/2/2024', '2026-02-30', 'tomorrow', '07/08/2025']
    if kind == 'amount':
        return ['1,234.50', '(12.00)', '2.665', '2.5', '1.0005', ' 7 ', 'abc', 'nan', '(1,000.255)', '-3.15', '0.05']
    if kind == 'email':
        return ['Ana.Silva@Example.ORG', ' bob@mail.example.edu ', 'a@b@c.org', 'no-at.example.net', '@x.org',
                'x@localhost', 'Mixed.Case@Sub.Domain.NET', 'with space@x.org', 'q@uni.EDU']
    if kind == 'country':
        names = list(params['ALIASES'])
        code = params['ALIASES'][names[0]]
        multi = next((name for name in names if ' ' in name), names[-1])
        return [names[0].upper(), f'  {names[1].title()} ', code.lower(), multi.replace(' ', '   '), 'atlantis',
                'zz', code, names[-1]]
    if kind == 'postcode':
        samples = {r'[A-Z]\d[A-Z]\d[A-Z]\d': ['h3z 2y7', 'H3Z2Y7', 'H3Z 2Y7X', '123456'],
                   r'\d{4}[A-Z]{2}': ['1234 ab', '1234AB', '1234ABC', '12 34 AB'],
                   r'[A-Z]{1,2}\d[A-Z\d]?\d[A-Z]{2}': ['qe4b 7nn', 'J9 3TT', 'J93TT!', 'XW8 4PL'],
                   r'[A-Z]\d{4}[A-Z]{3}': ['c1425 abc', 'C1425ABC', 'C1425ABCD', 'C 1425 ABC'],
                   r'\d{3}[A-Z]{2}\d': ['123 ab 4', '123AB4', '123AB45', '12AB4']}
        return samples[params['PATTERN']] + ['', '  ']
    if kind == 'name':
        first, second = params['PARTICLES'][:2]
        return [f'ana {first} silva', f'{first.upper()} costa', f'MARIA   {second}  souza', 'ana-luiza moraes',
                f'o\'NEIL {first}', '   ', 'x', f'LUIS {first.title()} {second.upper()} ROCHA']
    true_words, false_words = params['TRUE'], params['FALSE']
    return [true_words[0].upper(), f' {false_words[0]} ', true_words[-1], false_words[-1].title(), 'maybe', '',
            f'\t{true_words[0]}\n', 'unknown']


def render(text, params):
    for key, value in params.items():
        text = text.replace(f'@{key}@', value if isinstance(value, str) else repr(value))
    return text


def spec_text(kind, params):
    words = dict(params)
    if kind == 'country':
        words['NAMES'] = ', '.join(sorted(params['ALIASES']))
    if kind == 'name':
        words['WORDS'] = ', '.join(params['PARTICLES'])
    if kind == 'flag':
        words['TRUEWORDS'], words['FALSEWORDS'] = ', '.join(params['TRUE']), ', '.join(params['FALSE'])
    text = NORMALIZERS[kind]['spec']
    for key, value in words.items():
        text = text.replace(f'@{key}@', value if isinstance(value, str) else repr(value))
    return text


def evaluate(code, inputs):
    namespace = {}
    exec(compile(code, '<normalizer>', 'exec'), namespace)
    results = []
    for value in inputs:
        try:
            results.append({'ok': namespace['normalize'](value)})
        except Exception as error:  # a defect may crash; the grader reports it the same way
            results.append({'error': type(error).__name__})
    return results


PIPELINE = d('''
    """Clean a raw CSV: python3 pipeline.py RAW_CSV OUT_DIR writes clean.csv and rejects.csv."""
    import csv
    import importlib
    import sys
    from pathlib import Path

    HERE = Path(__file__).resolve().parent
    sys.path.insert(0, str(HERE))
    COLUMNS = @COLUMNS@


    def clean_row(row):
        """Normalized row and the names of columns whose value is invalid."""
        cleaned, invalid = {}, []
        for column in COLUMNS:
            value = importlib.import_module(f'normalizers.{column}').normalize(row[column])
            cleaned[column] = value
            if value is None:
                invalid.append(column)
        return cleaned, invalid


    def run(raw_path, out_dir):
        out_dir = Path(out_dir)
        out_dir.mkdir(parents=True, exist_ok=True)
        with open(raw_path, newline='') as handle:
            rows = list(csv.DictReader(handle))
        with open(out_dir / 'clean.csv', 'w', newline='') as clean, open(out_dir / 'rejects.csv', 'w', newline='') as rejects:
            good = csv.DictWriter(clean, ['row', *COLUMNS])
            bad = csv.writer(rejects)
            good.writeheader()
            bad.writerow(['row', 'columns'])
            for number, row in enumerate(rows, 1):
                cleaned, invalid = clean_row(row)
                if invalid:
                    bad.writerow([number, ' '.join(invalid)])
                else:
                    good.writerow({'row': number, **cleaned})


    if __name__ == '__main__':
        if len(sys.argv) != 3:
            print('usage: python3 pipeline.py RAW_CSV OUT_DIR', file=sys.stderr)
            raise SystemExit(2)
        run(sys.argv[1], sys.argv[2])
''')

PIPELINE_CHECK = d('''
    """Run the tests of one column: python3 check.py normalizers/NAME.py"""
    import subprocess
    import sys
    from pathlib import Path

    HERE = Path(__file__).resolve().parent


    def main(argv):
        if len(argv) != 2:
            print('usage: python3 check.py normalizers/NAME.py', file=sys.stderr)
            return 2
        name = Path(argv[1]).stem
        return subprocess.run([sys.executable, '-m', 'unittest', f'tests.test_{name}'], cwd=HERE).returncode


    if __name__ == '__main__':
        raise SystemExit(main(sys.argv))
''')


def clean_pipeline():
    tree = Tree('long_clean_pipeline')
    columns, spec, hidden = [], ['# Column rules', '',
                                 'Every normalizer returns the cleaned text, or None when the value is invalid.', ''], {}
    for kind_index, (kind, template) in enumerate(NORMALIZERS.items()):
        for instance, params in enumerate(template['params']):
            column = f'{NOUNS[kind_index * 5 + instance]}_{kind}'
            columns.append(column)
            good = render(template['code'], params)
            old, new = template['defects'][(instance + kind_index) % len(template['defects'])]
            if good.count(old) != 1:
                raise AssertionError(f'{column}: defect site must match once: {old!r}')
            bad = good.replace(old, new)
            inputs = normalizer_inputs(kind, params)
            expected, seeded = evaluate(good, inputs), evaluate(bad, inputs)
            if any('error' in result for result in expected):
                raise AssertionError(f'{column}: the reference normalizer raised')
            differing = [i for i, (a, b) in enumerate(zip(expected, seeded)) if a != b]
            if not differing:
                raise AssertionError(f'{column}: defect is invisible on the hidden inputs')
            header = f'"""Normalizer for the {column} column (rules in SPEC.md)."""\n'
            tree.fixture[f'normalizers/{column}.py'] = header + bad
            tree.reference[f'normalizers/{column}.py'] = header + good
            visible = [i for i in range(len(inputs)) if i not in differing][:1] + differing[:1]
            cases = '\n'.join(f'        self.assertEqual(normalize({inputs[i]!r}), {expected[i]["ok"]!r})'
                              for i in visible)
            tree.both(f'tests/test_{column}.py', d(f'''
                import unittest

                from normalizers.{column} import normalize


                class {''.join(part.title() for part in column.split('_'))}Tests(unittest.TestCase):
                    def test_examples(self):
            ''') + cases + '\n')
            spec += [f'## {column}', '', spec_text(kind, params), '']
            hidden[column] = [{'input': value, 'expected': result} for value, result in zip(inputs, expected)]
    tree.both('normalizers/__init__.py', '')
    tree.both('tests/__init__.py', '')
    tree.both('SPEC.md', '\n'.join(spec))
    tree.both('pipeline.py', PIPELINE.replace('@COLUMNS@', repr(tuple(columns))))
    tree.both('check.py', PIPELINE_CHECK)
    rng = random.Random('raw-rows')
    rows = []
    for number in range(40):
        # Mostly valid rows so clean.csv has content; every third row has one or two invalid values.
        row = {column: rng.choice([case['input'] for case in hidden[column] if case['expected']['ok'] is not None])
               for column in columns}
        if number % 3 == 2:
            for column in rng.sample(columns, rng.randint(1, 2)):
                row[column] = rng.choice([case['input'] for case in hidden[column] if case['expected']['ok'] is None])
        rows.append(row)
    raw = ','.join(columns) + '\n' + ''.join(
        ','.join('"' + row[column].replace('"', '""') + '"' for column in columns) + '\n' for row in rows)
    tree.both('data/raw.csv', raw)
    tree.expected = {'columns': hidden, 'raw': raw}
    return tree


PIPELINE_EXPECTED = d('''
    import json, tempfile
    from pathlib import Path
    import pipeline
    with tempfile.TemporaryDirectory() as out:
        pipeline.run('data/raw.csv', out)
        print(json.dumps({name: (Path(out) / name).read_text() for name in ('clean.csv', 'rejects.csv')}))
''')

# --------------------------------------------------------------------------- money rename

MONEY_OLD = d('''
    class Money:
        """An amount of integer cents in one currency."""

        def __init__(self, amount_cents, currency='USD'):
            if type(amount_cents) is not int:
                raise TypeError('amount_cents must be an int')
            self.amount_cents = amount_cents
            self.currency = currency

        def __add__(self, other):
            if other.currency != self.currency:
                raise ValueError('currency mismatch')
            return Money(self.amount_cents + other.amount_cents, self.currency)

        def __eq__(self, other):
            return isinstance(other, Money) and (self.amount_cents, self.currency) == (other.amount_cents, other.currency)

        def __repr__(self):
            return f'Money({self.amount_cents!r}, {self.currency!r})'

        def to_dict(self):
            """Wire format shared with other services."""
            return {'amount_cents': self.amount_cents, 'currency': self.currency}

        @classmethod
        def from_dict(cls, data):
            return cls(data['amount_cents'], data['currency'])
''')

MONEY_NEW = d('''
    class Money:
        """An amount of integer cents in one currency."""

        def __init__(self, currency, cents):
            if type(cents) is not int:
                raise TypeError('cents must be an int')
            self.currency = currency
            self.cents = cents

        def __add__(self, other):
            if other.currency != self.currency:
                raise ValueError('currency mismatch')
            return Money(self.currency, self.cents + other.cents)

        def __eq__(self, other):
            return isinstance(other, Money) and (self.currency, self.cents) == (other.currency, other.cents)

        def __repr__(self):
            return f'Money({self.currency!r}, {self.cents!r})'

        def to_dict(self):
            """Wire format shared with other services."""
            return {'amount_cents': self.cents, 'currency': self.currency}

        @classmethod
        def from_dict(cls, data):
            return cls(data['currency'], data['amount_cents'])
''')

# (import, old body, new body, call args); {n} names the function, {c} a constant, {cur} a currency.
MONEY_TEMPLATES = (
    ('Money', 'def unit_price_{n}():\n    return Money({c}).to_dict()',
     "def unit_price_{n}():\n    return Money('USD', {c}).to_dict()", [[]]),
    ('Money', "def line_total_{n}(qty):\n    price = Money({c}, '{cur}')\n    return Money(price.amount_cents * qty, price.currency).to_dict()",
     "def line_total_{n}(qty):\n    price = Money('{cur}', {c})\n    return Money(price.currency, price.cents * qty).to_dict()", [[3], [0]]),
    ('Money', "def label_{n}(cents):\n    m = Money(amount_cents=cents, currency='{cur}')\n    return f'{{m.amount_cents / 100:.2f}} {{m.currency}}'",
     "def label_{n}(cents):\n    m = Money(currency='{cur}', cents=cents)\n    return f'{{m.cents / 100:.2f}} {{m.currency}}'", [[1999], [5]]),
    ('Money', 'def total_{n}(values):\n    return sum((Money(v) for v in values), Money(0)).amount_cents',
     "def total_{n}(values):\n    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents", [[[1, 2, 'C']], [[]]]),
    ('Money', "def cheapest_{n}(values):\n    items = [Money(v, '{cur}') for v in values]\n    return min(items, key=lambda m: m.amount_cents).to_dict()",
     "def cheapest_{n}(values):\n    items = [Money('{cur}', v) for v in values]\n    return min(items, key=lambda m: m.cents).to_dict()", [[['C', 7, 300]]]),
    ('Money', "def field_{n}(cents):\n    m = Money(cents)\n    return getattr(m, 'amount_cents') + {c}",
     "def field_{n}(cents):\n    m = Money('USD', cents)\n    return getattr(m, 'cents') + {c}", [[10]]),
    ('Money', 'def roundtrip_{n}(payload):\n    m = Money.from_dict(payload)\n    return Money(m.amount_cents + {c}, m.currency).to_dict()',
     'def roundtrip_{n}(payload):\n    m = Money.from_dict(payload)\n    return Money(m.currency, m.cents + {c}).to_dict()',
     [[{'amount_cents': 100, 'currency': 'EUR'}]]),
    (None, "def wire_{n}(cents):\n    return {{'amount_cents': cents, 'currency': '{cur}', 'source': '{noun}'}}",
     "def wire_{n}(cents):\n    return {{'amount_cents': cents, 'currency': '{cur}', 'source': '{noun}'}}", [[42]]),
    ('M', 'def double_{n}(cents):\n    return (M(cents) + M(cents)).amount_cents',
     "def double_{n}(cents):\n    return (M('USD', cents) + M('USD', cents)).cents", [['C']]),
    ('money', "def make_{n}(cents):\n    return money.Money(cents, '{cur}').to_dict()",
     "def make_{n}(cents):\n    return money.Money('{cur}', cents).to_dict()", [['C']]),
    ('Money', 'def convert_{n}(cents, currency):\n    return Money(cents * {c} // 100, currency).to_dict()',
     'def convert_{n}(cents, currency):\n    return Money(currency, cents * {c} // 100).to_dict()', [[1000, 'BRL']]),
    ('Money', 'def discount_{n}(cents, pct):\n    m = Money(cents)\n    m.amount_cents -= m.amount_cents * pct // 100\n    return m.to_dict()',
     "def discount_{n}(cents, pct):\n    m = Money('USD', cents)\n    m.cents -= m.cents * pct // 100\n    return m.to_dict()", [['C', 15]]),
)
MONEY_IMPORTS = {'Money': 'from money import Money', 'M': 'from money import Money as M', 'money': 'import money'}

MONEY_CHECK = d('''
    """Exercise one billing module: python3 check.py billing/NAME.py"""
    import importlib
    import json
    import sys
    from pathlib import Path

    HERE = Path(__file__).resolve().parent
    sys.path.insert(0, str(HERE))
    import money  # noqa: E402


    def main(argv):
        if len(argv) != 2:
            print('usage: python3 check.py billing/NAME.py', file=sys.stderr)
            return 2
        try:
            sample = money.Money('USD', 1)
        except TypeError:
            sample = None
        if sample is None or getattr(sample, 'cents', None) != 1 or hasattr(sample, 'amount_cents'):
            print('money.Money is not migrated yet: expected Money(currency, cents) with a cents attribute',
                  file=sys.stderr)
            return 1
        name = Path(argv[1]).stem
        module = importlib.import_module(f'billing.{name}')
        calls = json.loads((HERE / 'samples.json').read_text())[name]
        for function, arguments in calls:
            print(f'{function}{tuple(arguments)!r} -> {getattr(module, function)(*arguments)!r}')
        return 0


    if __name__ == '__main__':
        raise SystemExit(main(sys.argv))
''')


def money_rename():
    tree = Tree('long_money_rename')
    tree.fixture['money.py'] = MONEY_OLD
    tree.reference['money.py'] = MONEY_NEW
    tree.both('billing/__init__.py', '')
    tree.both('check.py', MONEY_CHECK)
    tree.both('README.md', d('''
        Billing helpers built on money.Money. Each module under billing/ is used by a different service.
        Exercise one module with `python3 check.py billing/NAME.py`.
    '''))
    samples = {}
    for index, noun in enumerate(NOUNS):
        rng = random.Random(f'money-{index}')
        picks = sorted({index % 12, (index * 5 + 3) % 12, (index * 7 + 1) % 12})
        values = {'n': noun, 'c': rng.randint(5, 900), 'cur': rng.choice(('USD', 'EUR', 'BRL', 'JPY')), 'noun': noun}
        imports = sorted({MONEY_TEMPLATES[p][0] for p in picks} - {None}, key=('money', 'Money', 'M').index)
        header = ''.join(MONEY_IMPORTS[name] + '\n' for name in imports)
        old = [MONEY_TEMPLATES[p][1].format(**values) for p in picks]
        new = [MONEY_TEMPLATES[p][2].format(**values) for p in picks]
        prefix = header + '\n\n' if header else ''
        tree.fixture[f'billing/{noun}.py'] = prefix + '\n\n\n'.join(old) + '\n'
        tree.reference[f'billing/{noun}.py'] = prefix + '\n\n\n'.join(new) + '\n'
        # 'C' in a template's arguments stands for the module's constant.
        samples[noun] = [[old[i].split('(')[0].removeprefix('def '),
                          json.loads(json.dumps(args).replace('"C"', str(values['c'])))]
                         for i, p in enumerate(picks) for args in MONEY_TEMPLATES[p][3]]
    tree.both('samples.json', json.dumps(samples, indent=1, sort_keys=True) + '\n')
    return tree


MONEY_EXPECTED = d('''
    import importlib, json
    samples = json.load(open('samples.json'))
    out = []
    for module, calls in sorted(samples.items()):
        loaded = importlib.import_module(f'billing.{module}')
        for function, arguments in calls:
            out.append({'module': module, 'function': function, 'arguments': arguments,
                        'result': getattr(loaded, function)(*arguments)})
    print(json.dumps({'calls': out}))
''')

# --------------------------------------------------------------------------- CommonJS to ES modules

def camel(noun):
    return noun[:1].upper() + noun[1:]


def esm_module(index, noun, new):
    rng = random.Random(f'esm-{index}')
    a, b = rng.randint(2, 9), rng.randint(1, 50)
    style = index % 4
    scale, label, main = f'scale{camel(noun)}', f'label{camel(noun)}', f'{noun}Main'
    dep_index = index - 1 if index % 2 else index // 2 - 1
    dep = NOUNS[dep_index] if dep_index >= 0 else None
    dep_default = dep is not None and dep_index % 4 == 2
    lazy = dep is not None and index % 7 == 3
    dirname = index % 9 == 4
    head, prelude, body = [], [], []
    if dirname:
        head.append("import path from 'node:path';" if new else "const path = require('node:path');")
        if new:
            head.append("import { fileURLToPath } from 'node:url';")
            prelude.append('const __dirname = path.dirname(fileURLToPath(import.meta.url));')
    if dep is not None:
        dep_scale = f'scale{camel(dep)}'
        if dep_default:
            stmt = f"import {dep}Main from './{dep}.js';" if new else f"const {dep}Main = require('./{dep}');"
            use = f'{dep}Main(x)'
        elif index % 3 == 0:
            stmt = f"import * as {dep}Lib from './{dep}.js';" if new else f"const {dep}Lib = require('./{dep}');"
            use = f'{dep}Lib.{dep_scale}(x)'
        else:
            stmt = f"import {{ {dep_scale} }} from './{dep}.js';" if new else f"const {{ {dep_scale} }} = require('./{dep}');"
            use = f'{dep_scale}(x)'
        if lazy and not new:
            body.append(f'function {scale}(x) {{\n  {stmt}\n  return {use} * {a} + {b};\n}}')
        else:
            head.append(stmt)
            body.append(f'function {scale}(x) {{\n  return {use} * {a} + {b};\n}}')
    else:
        body.append(f'function {scale}(x) {{\n  return x * {a} + {b};\n}}')
    suffix = "' @' + path.basename(__dirname)" if dirname else f"'{noun[:3]}'"
    body.append(f"function {label}(text) {{\n  return String(text).split('').reverse().join('') + '-' + {suffix};\n}}")
    if style == 2:
        body.append(f'function {main}(x) {{\n  return {scale}(x) - {b};\n}}')
    exports = {0: (f'module.exports = {{ {scale}, {label} }};', f'export {{ {scale}, {label} }};'),
               1: (f'exports.{scale} = {scale};\nexports.{label} = {label};', f'export {{ {scale}, {label} }};'),
               2: (f'module.exports = {main};\nmodule.exports.{scale} = {scale};\nmodule.exports.{label} = {label};',
                   f'export default {main};\nexport {{ {scale}, {label} }};'),
               3: (f'module.exports.{scale} = {scale};\nmodule.exports.{label} = {label};',
                   f'export {{ {scale}, {label} }};')}[style]
    text = ''
    for block in (head, prelude):
        if block:
            text += '\n'.join(block) + '\n\n'
    return text + '\n\n'.join(body) + '\n\n' + exports[1 if new else 0] + '\n'


def cjs_to_esm():
    tree = Tree('long_cjs_to_esm')
    nouns = NOUNS[:40]
    tree.fixture['package.json'] = json.dumps({'name': 'tally-lib', 'version': '1.4.0', 'private': True}, indent=2) + '\n'
    tree.reference['package.json'] = json.dumps(
        {'name': 'tally-lib', 'version': '1.4.0', 'private': True, 'type': 'module'}, indent=2) + '\n'
    for index, noun in enumerate(nouns):
        tree.fixture[f'lib/{noun}.js'] = esm_module(index, noun, new=False)
        tree.reference[f'lib/{noun}.js'] = esm_module(index, noun, new=True)
    defaults = [noun for index, noun in enumerate(nouns) if index % 4 == 2]
    tree.fixture['index.js'] = 'module.exports = {\n' + ''.join(
        f"  ...require('./lib/{noun}'),\n" for noun in nouns) + ''.join(
        f"  {noun}Main: require('./lib/{noun}'),\n" for noun in defaults) + '};\n'
    tree.reference['index.js'] = ''.join(f"export * from './lib/{noun}.js';\n" for noun in nouns) + ''.join(
        f"export {{ default as {noun}Main }} from './lib/{noun}.js';\n" for noun in defaults)
    tree.both('check.mjs', d('''
        // Check one converted module: node check.mjs lib/NAME.js
        import { readFileSync } from 'node:fs';
        import path from 'node:path';
        import { pathToFileURL } from 'node:url';

        const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));
        if (pkg.type !== 'module') {
          console.error('package.json does not declare "type": "module" yet');
          process.exit(1);
        }
        const file = process.argv[2];
        if (!file) {
          console.error('usage: node check.mjs lib/NAME.js');
          process.exit(2);
        }
        const loaded = await import(pathToFileURL(path.resolve(file)).href);
        const names = Object.keys(loaded).sort();
        if (!names.length) {
          console.error(`${file}: no exports`);
          process.exit(1);
        }
        console.log(`ok ${file}: ${names.join(', ')}`);
    '''))
    tree.both('README.md', d('''
        Small numeric and text helpers used by the reporting jobs. Every module in lib/ is re-exported by index.js.
        Check one module with `node check.mjs lib/NAME.js`.
    '''))
    calls = []
    for index, noun in enumerate(nouns):
        calls.append({'module': noun, 'export': f'scale{camel(noun)}', 'arguments': [index + 3]})
        calls.append({'module': noun, 'export': f'label{camel(noun)}', 'arguments': [f'ab{index}']})
        if index % 4 == 2:
            calls.append({'module': noun, 'export': 'default', 'arguments': [index]})
    tree.expected = {'calls': calls, 'index_exports': sorted(
        [f'scale{camel(n)}' for n in nouns] + [f'label{camel(n)}' for n in nouns] + [f'{n}Main' for n in defaults])}
    return tree


def esm_expected(tree, scratch):
    base = materialize(tree, 'fixtures', scratch)
    script = ("const calls = JSON.parse(process.argv[1]); const out = calls.map((c) => {"
              " const m = require('./lib/' + c.module); const f = c.export === 'default' ? m : m[c.export];"
              " return f(...c.arguments); }); console.log(JSON.stringify(out));")
    node = shutil.which('node')
    if node is None:
        raise RuntimeError('node is required to compute the long_cjs_to_esm expectations')
    run = subprocess.run([node, '-e', script, json.dumps(tree.expected['calls'])], cwd=base, capture_output=True,
                         text=True, timeout=60, check=True)
    for call, result in zip(tree.expected['calls'], json.loads(run.stdout)):
        call['result'] = result


def build(root):
    scratch_root = root / '.long-fixture-scratch'
    if scratch_root.exists():
        shutil.rmtree(scratch_root)
    try:
        handlers = handler_migration()
        handlers.expected = run_python(materialize(handlers, 'fixtures', scratch_root), HANDLER_EXPECTED)
        pipeline = clean_pipeline()
        pipeline.expected['pipeline'] = run_python(materialize(pipeline, 'reference', scratch_root), PIPELINE_EXPECTED)
        rename = money_rename()
        rename.expected = run_python(materialize(rename, 'fixtures', scratch_root), MONEY_EXPECTED)
        esm = cjs_to_esm()
        esm_expected(esm, scratch_root)
    finally:
        shutil.rmtree(scratch_root, ignore_errors=True)
    for tree in (handlers, pipeline, rename, esm):
        tree.write(root)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--root', type=Path, default=ROOT,
                        help='tree that receives fixtures/, reference/ and graders/ (default: this directory)')
    build(parser.parse_args(argv).root.resolve())
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
