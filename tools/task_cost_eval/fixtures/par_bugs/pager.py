def page_count(total_items, per_page):
    """Number of pages needed to show total_items; 0 items need 0 pages."""
    return total_items // per_page
