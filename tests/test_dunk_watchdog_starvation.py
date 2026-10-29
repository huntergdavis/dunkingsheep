"""Real SQLite fixtures for idle-capacity escalation and partial delivery retries."""
import json
import os
from unittest.mock import patch

from test_dunk_watchdog import WatchdogCase, NOW, MIN, wd


class StarvationTests(WatchdogCase):
    def setup_rule(self, **overrides):
        self.fleet.panes = [{'pane_id': '%55', 'agent_status': 'idle'},
                            {'pane_id': '%56', 'agent_status': 'done'},
                            {'pane_id': '%15', 'agent_status': 'working'}]
        self.fleet.task('t1', 'blocked', NOW - 60 * MIN, assignees=['%33'],
                        block_reason='Owner approval required')
        with open(os.path.join(self.fleet.dir, 'superusers.json'), 'w') as f:
            json.dump({'panes': ['%2', '%15']}, f)
        opts = dict(worker_panes=['%55', '%56'], superuser_panes=['%2', '%15'],
                    quiet_minutes=10, min_empty_workers=2, must_hold_runs=2)
        opts.update(overrides)
        return self.fleet.config(fleet__assignment_starvation=opts)

    def test_blocked_backlog_with_empty_workers_even_while_reviews_complete(self):
        config = self.setup_rule()
        self.fleet.task_event('review', 'completed', NOW - MIN, actor='%31')
        findings = self.fleet.evaluate(config)
        self.assertEqual(len(findings), 1)
        evidence = findings[0]['evidence']
        self.assertEqual(evidence['idle_workers'], ['%55', '%56'])
        self.assertEqual(evidence['blocked_tasks'], ['t1'])
        self.assertEqual(evidence['done_records_in_window'], 1)
        self.assertIn('preserve owner holds', findings[0]['summary'])

    def test_busy_unknown_missing_recent_and_assigned_do_not_count(self):
        config = self.setup_rule()
        for status in ['working', 'blocked']:
            self.fleet.panes[0]['agent_status'] = status
            self.assertEqual(self.fleet.evaluate(config), [])
        self.fleet.panes[0]['agent_status'] = 'idle'
        self.fleet.sql('metrics.db', "INSERT INTO events(at,kind,pane_id,source) VALUES(?,?,?,?)",
                       (NOW - MIN, 'turn.end', '%55', 'pi-extension'))
        self.assertEqual(self.fleet.evaluate(config), [])
        self.fleet.sql('metrics.db', 'DELETE FROM events')
        self.fleet.lease('other', '%55', NOW - 2*MIN, NOW - MIN, NOW + MIN)
        self.assertEqual(self.fleet.evaluate(config), [])
        self.fleet.sql('tasks.db', 'DELETE FROM task_leases')
        self.fleet.task('ready', 'open', NOW - MIN, assignees=['%55'])
        self.assertEqual(self.fleet.evaluate(config), [])

    def test_no_backlog_private_backlog_and_fresh_backlog_do_not_alert(self):
        config = self.setup_rule()
        for sql, params in [
            ("UPDATE tasks SET status='done'", ()),
            ("UPDATE tasks SET status='blocked',scope='%33'", ()),
            ("UPDATE tasks SET scope='global',created_at=?", (NOW - MIN,)),
        ]:
            self.fleet.sql('tasks.db', sql, params)
            self.assertEqual(self.fleet.evaluate(config), [])

    def test_future_events_and_expired_leases_do_not_mask_current_starvation(self):
        config = self.setup_rule()
        self.fleet.lease('old', '%55', NOW - 60*MIN, NOW - 40*MIN, NOW - MIN)
        self.fleet.task_event('future', 'claimed', NOW + MIN, actor='%55')
        self.fleet.sql('metrics.db', "INSERT INTO events(at,kind,pane_id) VALUES(?,?,?)",
                       (NOW + MIN, 'turn.start', '%55'))
        self.assertEqual(len(self.fleet.evaluate(config)), 1)

    def test_unknown_is_visibility_gap_not_idle_and_missing_pane_is_not_capacity(self):
        config = self.setup_rule()
        self.fleet.panes[0]['agent_status'] = None
        evidence = self.fleet.evaluate(config)[0]['evidence']
        self.assertEqual(evidence['idle_workers'], ['%56'])
        self.assertEqual(evidence['activity_unknown_workers'], ['%55'])
        self.fleet.panes.pop(0)
        self.assertEqual(self.fleet.evaluate(config), [])

    def test_private_assigned_work_suppresses_empty_queue(self):
        config = self.setup_rule()
        self.fleet.task('private', 'open', NOW - 60*MIN, assignees=['%55'])
        self.fleet.sql('tasks.db', "UPDATE tasks SET scope='%55' WHERE id='private'")
        self.assertEqual(self.fleet.evaluate(config), [])

    def test_daemon_inventory_is_used_and_malformed_reply_fails_visible(self):
        config = self.setup_rule()
        config['panes']['source'] = 'daemon'
        with patch('dunk_client.FlockClient') as client:
            client.return_value.call.return_value = {'panes': self.fleet.panes}
            result = wd.evaluate(config, NOW, host_sampler=self.fleet.host)
            self.assertEqual(len(result['findings']), 1)
            client.return_value.call.assert_called_once_with('list_panes')
            client.return_value.call.return_value = {}
            result = wd.evaluate(config, NOW, host_sampler=self.fleet.host)
            self.assertTrue(result['errors'])
            self.assertEqual(result['findings'], [])

    def test_hold_group_delivery_dedupe_and_recovery(self):
        config = self.live(self.setup_rule())
        self.seed_state(config)
        report, sent = self.fleet.run(config)
        self.assertEqual(sent, [])
        self.assertEqual(report['decisions'][0]['vp'], 'hold')
        report, sent = self.fleet.run(config, now=NOW + MIN)
        self.assertEqual(sorted(x[0] for x in sent), ['%15', '%2'])
        self.assertEqual(report['sent'], ['superusers'])
        self.assertEqual(self.fleet.run(config, now=NOW + 2*MIN)[1], [])
        self.fleet.sql('tasks.db', "UPDATE tasks SET status='done'")
        report, sent = self.fleet.run(config, now=NOW + 3*MIN)
        self.assertTrue(report['recovered']['vp'])
        state = wd.load_state(config['state_path'])[0]
        self.assertEqual(state['superuser_deliveries'], {})

    def test_partial_failure_retries_only_failed_superuser(self):
        config = self.live(self.setup_rule(must_hold_runs=1))
        self.seed_state(config)
        sent = []
        fail = {'%2'}
        def sender(to, subject, body):
            sent.append(to)
            if to in fail:
                raise RuntimeError('injected delivery failure')
        def run(at):
            return wd.run(config, at, send=sender, panes_provider=lambda:self.fleet.panes,
                          host_sampler=self.fleet.host)
        report = run(NOW)
        self.assertNotIn('superusers', report['sent'])
        self.assertEqual(sent.count('%15'), 1)
        fail.clear(); sent.clear()
        report = run(NOW + MIN)
        self.assertEqual(sent, ['%2'])
        self.assertEqual(report['sent'], ['superusers'])

    def test_unprotected_recipient_refuses_whole_group(self):
        config = self.live(self.setup_rule(must_hold_runs=1, superuser_panes=['%2', '%99']))
        self.seed_state(config)
        report, sent = self.fleet.run(config)
        self.assertTrue(any('not all current superusers' in p for p in report['problems']))
        self.assertNotIn('%2', [x[0] for x in sent])
        self.assertNotIn('%99', [x[0] for x in sent])

    def test_legacy_config_migrates_disabled_and_dry_run_never_delivers(self):
        config = self.setup_rule()
        del config['rules']['fleet.assignment_starvation']
        wd.validate_config(config)
        self.assertFalse(config['rules']['fleet.assignment_starvation']['enabled'])
        self.fleet.sql("tasks.db", "DELETE FROM tasks")
        config = self.live(self.setup_rule(must_hold_runs=1))
        report, sent = self.fleet.run(config, dry=True)
        self.assertEqual(sent, [])
        self.assertFalse(os.path.exists(config['state_path']))
