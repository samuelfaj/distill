def page_count(total_items, per_page):
    return -(-total_items // per_page)
