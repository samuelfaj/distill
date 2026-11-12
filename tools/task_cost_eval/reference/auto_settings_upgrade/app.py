def font_label(settings):
    return f"{settings['font']['family']} {settings['font']['size']}pt"


def plugin_names(settings):
    return list(settings['plugins'])
