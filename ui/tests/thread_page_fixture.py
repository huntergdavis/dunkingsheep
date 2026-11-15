"""Convert existing synthetic browser fixtures to the new projected thread contract.

This is test data shaping only, never a product fallback or a daemon query.
"""
from datetime import datetime
from urllib.parse import parse_qs, urlsplit


def project(route, value, all_items=None):
    q = parse_qs(urlsplit(route.request.url).query)
    mode = q.get('thread_mode', [None])[0]
    if not mode or not isinstance(value, dict) or 'items' not in value:
        return value
    view = q.get('filter', ['human_threads'])[0]
    root = q.get('root_id', [None])[0]
    items = [dict(row, sort_at=row.get('sort_at', datetime.fromisoformat(row['created_at'].replace('Z', '+00:00')).timestamp())) for row in (all_items if mode == 'messages' and all_items is not None else value['items'])]
    result = dict(thread_mode=mode, source_filter=value.get('source_filter'), include_internal=value.get('include_internal'), view_revision=value.get('view_revision'), fence_id=999999, reload_from_top=False, next_cursor=value.get('next_before'))
    if mode == 'messages':
        result['root_id'] = root
        result['messages'] = sorted([row for row in items if (row['message_id'] if view == 'historical' else row.get('root_id')) == root], key=lambda r: (r['sort_at'], int(r['message_id'][1:])))
        result['next_cursor'] = None
    else:
        groups = {}
        for row in items:
            key = row['message_id'] if view == 'historical' else row.get('root_id')
            groups.setdefault(key, []).append(row)
        heads = []
        for key, messages in groups.items():
            latest = max(messages, key=lambda r: (r['sort_at'], int(r['message_id'][1:])))
            preview = latest.pop('body', '')[:240]
            heads.append(dict(thread_id=key, latest=dict(latest, preview=preview), message_count=len(messages), unread_count=len(messages)))
        result['threads'] = sorted(heads, key=lambda h: (h['latest']['sort_at'], int(h['latest']['message_id'][1:])), reverse=True)
    return result
