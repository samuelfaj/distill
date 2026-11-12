def font_label(settings):
    return settings['font'] + 'pt'


def plugin_names(settings):
    return [name for name in settings['plugins'].split(',') if name]
