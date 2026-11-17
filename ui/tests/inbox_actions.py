def open_thread(page, text=None):
    back = page.get_by_role('button', name='Back to inbox', exact=True)
    if back.is_visible():
        back.click()
    rows = page.locator('.owner-inbox-row')
    (rows.filter(has_text=text) if text else rows.first).click()
