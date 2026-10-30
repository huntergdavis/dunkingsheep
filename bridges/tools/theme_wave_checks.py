"""Pure geometry predicates shared by browser evidence and dependency-free tests."""

OPEN_ICON_CHECK = 'Open Appearance close icon unobscured'
ADVISORY_WIDTHS = (320,)


def target_meets_minimum(geometry):
    return bool(geometry['fragments']) and all(r['width'] >= 24 and r['height'] >= 24
        for r in [geometry['union'], *geometry['fragments']])


def visible_focus(geometry):
    return (geometry['focused'] and geometry['focusVisible'] and
            geometry['outlineStyle'] not in ('none', 'hidden') and geometry['outlineWidth'] >= 2 and
            geometry['ringContained'] and geometry['unobscured'])


