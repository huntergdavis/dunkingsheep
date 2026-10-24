"""Configurable provider-usage runner (owner #1110)."""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_usage as usage  # noqa: E402
import usage_providers as providers  # noqa: E402


def make_script(directory, name, payload=None, exit_code=0, sleep=0, stderr=""):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("import json, os, sys, time\n")
        if sleep:
            handle.write(f"time.sleep({sleep})\n")
        if stderr:
            handle.write(f"sys.stderr.write({stderr!r})\n")
        if payload is not None:
            handle.write(f"print(json.dumps({payload!r}))\n")
        handle.write(f"sys.exit({exit_code})\n")
    return path


class UsageRunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def spec(self, payload=None, **kwargs):
        path = make_script(self.tmp.name, "cmd.py", payload=payload, **kwargs)
        return {"command": f"{sys.executable} {path}", "timeout_s": 5,
                "parse": {"windows": "windows", "meaning": "meaning",
                          "measured_at": "measured_at"}}

    def test_ok_result_and_parse(self):
        payload = {"provider": "x", "status": "ok", "meaning": "percent_used",
                   "measured_at": 1, "windows": [{"name": "weekly",
                                                  "percent_used": 15.0,
                                                  "percent_remaining": 85.0}]}
        result = usage.run_provider("x", self.spec(payload))
        self.assertEqual("ok", result["status"])
        self.assertEqual(15.0, result["parsed"]["windows"][0]["percent_used"])
        self.assertEqual(1, result["parsed"]["measured_at"])

    def test_failed_command_surfaces_stderr(self):
        result = usage.run_provider("x", self.spec(exit_code=3, stderr="boom"))
        self.assertEqual("error", result["status"])
        self.assertEqual("boom", result["stderr"])
        self.assertEqual(3, result["exit_code"])
        self.assertIn("UNKNOWN", usage.render_text([result]))

    def test_command_can_return_unknown_with_reason(self):
        payload = {"provider": "x", "status": "unknown", "meaning": "unknown",
                   "measured_at": 1, "reason": "no endpoint"}
        result = usage.run_provider("x", self.spec(payload, exit_code=1))
        self.assertEqual("unknown", result["status"])
        text = usage.render_text([result])
        self.assertIn("UNKNOWN", text)
        self.assertIn("no endpoint", text)

    def test_timeout_is_an_error(self):
        path = make_script(self.tmp.name, "slow.py", payload={}, sleep=2)
        spec = {"command": f"{sys.executable} {path}", "timeout_s": 0.2}
        result = usage.run_provider("x", spec)
        self.assertEqual("error", result["status"])
        self.assertIn("timed out", result["error"])

    def test_invalid_json_is_an_error(self):
        path = os.path.join(self.tmp.name, "bad.py")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("print('not json')\n")
        result = usage.run_provider("x", {"command": f"{sys.executable} {path}",
                                          "timeout_s": 5})
        self.assertEqual("error", result["status"])
        self.assertIn("canonical JSON", result["error"])

    def test_missing_parse_path_is_an_error(self):
        payload = {"status": "ok", "meaning": "percent_used", "measured_at": 1,
                   "windows": []}
        spec = self.spec(payload)
        spec["parse"] = {"missing": "not.there"}
        result = usage.run_provider("x", spec)
        self.assertEqual("error", result["status"])
        self.assertIn("not.there", result["error"])

    def test_no_cache_runs_the_command_each_time(self):
        counter = os.path.join(self.tmp.name, "count")
        path = os.path.join(self.tmp.name, "count.py")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("import json, sys\n")
            handle.write(f"open({counter!r}, 'a').write('x')\n")
            handle.write("print(json.dumps({'status':'ok','meaning':'percent_used',"
                         "'measured_at':1}))\n")
        spec = {"command": f"{sys.executable} {path}", "timeout_s": 5}
        usage.run_provider("x", spec)
        usage.run_provider("x", spec)
        with open(counter, encoding="utf-8") as handle:
            self.assertEqual(2, len(handle.read()))

    def test_max_age_is_passed_to_the_command(self):
        path = os.path.join(self.tmp.name, "age.py")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("import json, os\n")
            handle.write("print(json.dumps({'status':'ok','meaning':'x','measured_at':1,"
                         "'age':os.environ.get('DUNKINGSHEEP_USAGE_MAX_AGE_S')}))\n")
        spec = {"command": f"{sys.executable} {path}", "timeout_s": 5,
                "max_age_s": 77}
        result = usage.run_provider("x", spec)
        self.assertEqual(77.0, float(result["age"]))

    def test_config_validation(self):
        path = os.path.join(self.tmp.name, "usage.json")
        with self.assertRaises(usage.UsageError):
            usage.load_config(path)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump({"schema": 1, "providers": {}}, handle)
        with self.assertRaises(usage.UsageError):
            usage.load_config(path)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump({"schema": 99, "providers": {"x": {"command": "true"}}}, handle)
        with self.assertRaises(usage.UsageError):
            usage.load_config(path)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump({"schema": 1, "providers": {"x": {"command": 1}}}, handle)
        with self.assertRaises(usage.UsageError):
            usage.load_config(path)

    def test_unknown_provider_is_a_clear_error(self):
        config = {"schema": 1, "providers": {"a": {"command": "true"}}}
        with self.assertRaisesRegex(usage.UsageError, "unknown provider"):
            usage.run(config, only="b")


class ProviderParserTests(unittest.TestCase):
    def test_nested_rate_limits_is_found(self):
        event = {"timestamp": "2026-10-21T00:00:00Z",
                 "payload": {"type": "token_count",
                             "rate_limits": {"primary": {"used_percent": 15.0}}}}
        self.assertEqual(15.0, providers._find_rate_limits(event)["primary"]["used_percent"])

    def test_stat_parses_k_suffix(self):
        text = "Total Cost $1.25\nInput 36.9K\nOutput 11"
        self.assertEqual(1.25, providers._stat(text, "Total Cost"))
        self.assertEqual(36900.0, providers._stat(text, "Input"))
        self.assertEqual(11.0, providers._stat(text, "Output"))


if __name__ == "__main__":
    unittest.main()
