def build(service, order, currency='USD', note=''):
    quote = service.quote(order, currency)
    quote['invoice_note'] = note
    return quote
