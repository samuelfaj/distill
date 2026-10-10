#!/usr/bin/env python3
import copy
import sys
from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        query, = import_fresh(fixture_dir(argv,'ultra_heldout_querycontrol'),'query')
    except Exception as error:
        return finish(['import: '+repr(error)])
    pairs = [('z','a b'),('A','/~'),('a','+'),('a',''),('a','+'),('ignored',None),('','x'),('é','雪')]
    saved = copy.deepcopy(pairs)
    expected = '=x&A=%2F~&a=&a=%2B&a=%2B&z=a%20b&%C3%A9=%E9%9B%AA'
    attempt(failures,'canonical encoding',lambda: query.encode_query(pairs),expected)
    attempt(failures,'generator encoding',lambda: query.encode_query(iter(pairs)),expected)
    attempt(failures,'empty encoding',lambda: query.encode_query([]),'')
    attempt(failures,'None omissions',lambda: query.encode_query([('x',None)]),'')
    attempt(failures,'safe punctuation',lambda: query.encode_query([('AZaz09-._~','&=?#')]),'AZaz09-._~=%26%3D%3F%23')
    for invalid in ([(1,'x')],[('x',1)],[(None,None)],[('x',True)]):
        attempt(failures,'type validation',lambda: query.encode_query(invalid),raises=ValueError)
    secrets = [('Token','s'),('token',None),('mytoken','public'),('STRASSE','hidden'),('Straße','hidden'),('other','ok')]
    saved_secrets = copy.deepcopy(secrets)
    redacted = [('Token','[REDACTED]'),('token','[REDACTED]'),('mytoken','public'),('STRASSE','[REDACTED]'),('Straße','[REDACTED]'),('other','ok')]
    attempt(failures,'whole casefold redaction',lambda: query.redact_pairs(secrets,['token','straße']),redacted)
    attempt(failures,'generator redaction',lambda: query.redact_pairs(iter(secrets),iter(['token','straße'])),redacted)
    attempt(failures,'no sensitive keys',lambda: query.redact_pairs(secrets,[]),secrets)
    attempt(failures,'empty redaction',lambda: query.redact_pairs([],['token']),[])
    def detached():
        result = query.redact_pairs(secrets,[])
        result.append(('extra','value'))
        return secrets
    attempt(failures,'detached result',detached,saved_secrets)
    attempt(failures,'inputs unchanged',lambda: (pairs,secrets),(saved,saved_secrets))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
