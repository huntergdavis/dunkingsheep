use serde_json::{json, Value};

use super::{advance_streaks, cooling, dedupe_key, plan, SOURCE_SELF};

// Generated from the exact Python AST functions/constants; oracle source SHA256: 44502f8bf243efd9976e40587db8b4dbdbb59414078aa90ba4125ca37db7d301.
const PYTHON_ORACLE: &str = r##"{
  "source": "dunk_watchdog.py:1530-1623 exact AST pure functions and source constants",
  "cases": [
    {
      "name": "stable_rule_key",
      "function": "dedupe_key",
      "args": [
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-rule"
        }
      ],
      "expected": "host.disk_full|root",
      "unchanged": true
    },
    {
      "name": "streak_spacing_159.0",
      "function": "advance_streaks",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "streaks": {
            "host.disk_full|root": {
              "count": 3,
              "last_counted": 100.0,
              "extra": true
            },
            "gone": {
              "count": 9,
              "last_counted": 100.0
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        159.0
      ],
      "expected": {
        "host.disk_full|root": {
          "count": 3,
          "last_counted": 100.0,
          "extra": true
        }
      },
      "unchanged": true
    },
    {
      "name": "streak_spacing_160.0",
      "function": "advance_streaks",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "streaks": {
            "host.disk_full|root": {
              "count": 3,
              "last_counted": 100.0,
              "extra": true
            },
            "gone": {
              "count": 9,
              "last_counted": 100.0
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        160.0
      ],
      "expected": {
        "host.disk_full|root": {
          "count": 4,
          "last_counted": 160.0
        }
      },
      "unchanged": true
    },
    {
      "name": "streak_spacing_161.0",
      "function": "advance_streaks",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "streaks": {
            "host.disk_full|root": {
              "count": 3,
              "last_counted": 100.0,
              "extra": true
            },
            "gone": {
              "count": 9,
              "last_counted": 100.0
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        161.0
      ],
      "expected": {
        "host.disk_full|root": {
          "count": 4,
          "last_counted": 161.0
        }
      },
      "unchanged": true
    },
    {
      "name": "streak_new",
      "function": "advance_streaks",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {},
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        10.0
      ],
      "expected": {
        "host.disk_full|root": {
          "count": 1,
          "last_counted": 10.0
        }
      },
      "unchanged": true
    },
    {
      "name": "streak_reset",
      "function": "advance_streaks",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "streaks": {
            "host.disk_full|root": {
              "count": 3,
              "last_counted": 100.0
            }
          }
        },
        [],
        160.0
      ],
      "expected": {},
      "unchanged": true
    },
    {
      "name": "cool_none",
      "function": "_cooling",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "alerts": {}
        },
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-rule"
        },
        100.0
      ],
      "expected": false,
      "unchanged": true
    },
    {
      "name": "cool_null",
      "function": "_cooling",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": null
            }
          }
        },
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-rule"
        },
        100.0
      ],
      "expected": false,
      "unchanged": true
    },
    {
      "name": "cool_before",
      "function": "_cooling",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": 100.0
            }
          }
        },
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-rule"
        },
        399.0
      ],
      "expected": true,
      "unchanged": true
    },
    {
      "name": "cool_exact",
      "function": "_cooling",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": 100.0
            }
          }
        },
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-rule"
        },
        400.0
      ],
      "expected": false,
      "unchanged": true
    },
    {
      "name": "cool_once",
      "function": "_cooling",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": 100.0
            }
          }
        },
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-rule",
          "once": true
        },
        100000.0
      ],
      "expected": true,
      "unchanged": true
    },
    {
      "name": "cool_self",
      "function": "_cooling",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": 100.0
            }
          }
        },
        {
          "rule": "host.disk_full",
          "key": "root",
          "class": "outage",
          "severity": "critical",
          "source": "watchdog-self",
          "once": true
        },
        101.0
      ],
      "expected": false,
      "unchanged": true
    },
    {
      "name": "order_caps",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          {
            "rule": "host.disk_full",
            "key": "second",
            "class": "money",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          {
            "rule": "host.disk_full",
            "key": "low",
            "class": "security",
            "severity": "info",
            "source": "watchdog-rule"
          },
          {
            "rule": "fleet.assignment_starvation",
            "key": "hold",
            "class": "ops",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {
          "fleet.assignment_starvation|hold": {
            "count": 1
          }
        },
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "no",
          "streak": 1
        },
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "second",
            "class": "money",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "rate",
          "owner": "global-cap",
          "streak": 1
        },
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "low",
            "class": "security",
            "severity": "info",
            "source": "watchdog-rule"
          },
          "vp": "below-min-severity",
          "owner": "below-min-severity",
          "streak": 1
        },
        {
          "finding": {
            "rule": "fleet.assignment_starvation",
            "key": "hold",
            "class": "ops",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "hold",
          "owner": null,
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "disabled",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": false,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "disabled",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "class_disabled",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "disabled",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "below_owner_severity",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "warning",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "warning",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "below-min-severity",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "run_block",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        "state-loss"
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "state-loss",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "pending",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {
            "host.disk_full|root": {
              "confirmed_at": null
            }
          },
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "dedupe",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "cooling",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {
            "host.disk_full|root": {
              "confirmed_at": 900.0
            }
          },
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "dedupe",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "cooldown_exact",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {
            "host.disk_full|root": {
              "confirmed_at": 700.0
            }
          },
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "no",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "hour_exact",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": [
            6400.0
          ]
        },
        {},
        10000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "no",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "hour_inside",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": [
            6400.001
          ]
        },
        {},
        10000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "global-cap",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "hour_budget_exhausted",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": [
            9999.0,
            9998.0
          ]
        },
        {},
        10000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "global-cap",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "self_bypasses_severity_and_cooling",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "info",
            "source": "watchdog-self"
          }
        ],
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": 999.0
            }
          },
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "info",
            "source": "watchdog-self"
          },
          "vp": "no",
          "owner": null,
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "ineligible_class",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "ops",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "ops",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": null,
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "ineligible_rule",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "fleet.assignment_starvation",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {
          "fleet.assignment_starvation|root": {
            "count": 3
          }
        },
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "fleet.assignment_starvation",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": null,
          "streak": 3
        }
      ],
      "unchanged": true
    },
    {
      "name": "immutable_once",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule",
            "once": true
          }
        ],
        {
          "alerts": {
            "host.disk_full|root": {
              "last_alerted": 1.0
            }
          },
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        100000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule",
            "once": true
          },
          "vp": "dedupe",
          "owner": "no",
          "streak": 1
        }
      ],
      "unchanged": true
    },
    {
      "name": "owner_budget_duplicate_findings",
      "function": "plan",
      "args": [
        {
          "min_run_spacing_seconds": 60,
          "max_alerts_per_run": 1,
          "rules": {
            "fleet.assignment_starvation": {
              "must_hold_runs": 2,
              "cooldown_minutes": 10
            },
            "host.disk_full": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "security.secret_text": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            },
            "money.spend_projection": {
              "must_hold_runs": 1,
              "cooldown_minutes": 5
            }
          },
          "escalation": {
            "vp": {
              "min_severity": "warning"
            },
            "owner": {
              "enabled": true,
              "classes": [
                "security",
                "outage",
                "money"
              ],
              "min_severity": "critical",
              "max_pages_per_hour": 1
            }
          }
        },
        [
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          }
        ],
        {
          "alerts": {},
          "owner_pages": {},
          "owner_page_times": []
        },
        {},
        1000.0,
        null
      ],
      "expected": [
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "no",
          "owner": "no",
          "streak": 1
        },
        {
          "finding": {
            "rule": "host.disk_full",
            "key": "root",
            "class": "outage",
            "severity": "critical",
            "source": "watchdog-rule"
          },
          "vp": "rate",
          "owner": "global-cap",
          "streak": 1
        }
      ],
      "unchanged": true
    }
  ]
}"##;

fn config() -> Value {
    json!({
        "min_run_spacing_seconds": 60,
        "max_alerts_per_run": 1,
        "rules": {
            "fleet.assignment_starvation": {"must_hold_runs": 2, "cooldown_minutes": 10},
            "host.disk_full": {"must_hold_runs": 1, "cooldown_minutes": 5},
            "security.secret_text": {"must_hold_runs": 1, "cooldown_minutes": 5},
            "money.spend_projection": {"must_hold_runs": 1, "cooldown_minutes": 5}
        },
        "escalation": {
            "vp": {"min_severity": "warning"},
            "owner": {
                "enabled": true,
                "classes": ["security", "outage", "money"],
                "min_severity": "critical",
                "max_pages_per_hour": 1
            }
        }
    })
}

fn finding(rule: &str, key: &str, class: &str, severity: &str) -> Value {
    json!({"rule": rule, "key": key, "class": class, "severity": severity, "source": "watchdog-rule"})
}

#[test]
fn stable_key_and_streak_boundaries_resets_and_duplicate_order() {
    let item = finding("host.disk_full", "root", "ops", "critical");
    assert_eq!(dedupe_key(&item), "host.disk_full|root");
    let config = config();
    let state = json!({"streaks": {"host.disk_full|root": {"count": 3, "last_counted": 100, "kept": true}, "host.disk_full|gone": {"count": 9, "last_counted": 100}}});
    let unchanged = advance_streaks(&config, &state, std::slice::from_ref(&item), 159.0);
    assert_eq!(
        unchanged["host.disk_full|root"],
        json!({"count": 3, "last_counted": 100, "kept": true})
    );
    let advanced = advance_streaks(&config, &state, &[item.clone(), item.clone()], 160.0);
    assert_eq!(
        advanced,
        json!({"host.disk_full|root": {"count": 4, "last_counted": 160.0}})
    );
    assert!(advanced.get("host.disk_full|gone").is_none());
    assert_eq!(
        advance_streaks(&config, &json!({}), &[item], 10.0)["host.disk_full|root"]["count"],
        1
    );
}

#[test]
fn cooldown_has_self_once_and_strict_boundary_semantics() {
    let config = config();
    let ordinary = finding("host.disk_full", "root", "ops", "critical");
    let state = json!({"alerts": {"host.disk_full|root": {"last_alerted": 100.0}}});
    assert!(cooling(&config, &state, &ordinary, 399.0));
    assert!(!cooling(&config, &state, &ordinary, 400.0));
    let mut once = ordinary.clone();
    once["once"] = json!(true);
    assert!(cooling(&config, &state, &once, 100_000.0));
    once["source"] = json!(SOURCE_SELF);
    assert!(!cooling(&config, &state, &once, 101.0));
}

#[test]
fn plan_preserves_order_reasons_caps_and_inputs() {
    let config = config();
    let findings = vec![
        finding("host.disk_full", "first", "outage", "critical"),
        finding("host.disk_full", "second", "money", "critical"),
        finding("host.disk_full", "low", "security", "info"),
        finding("fleet.assignment_starvation", "held", "ops", "critical"),
    ];
    let state = json!({"alerts": {}, "owner_pages": {}, "owner_page_times": [6400.0]});
    let streaks = json!({"fleet.assignment_starvation|held": {"count": 1}});
    let original_findings = findings.clone();
    let original_state = state.clone();
    let decisions = plan(&config, &findings, &state, &streaks, 10_000.0, None);
    assert_eq!(
        decisions
            .iter()
            .map(|d| d["finding"]["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second", "low", "held"]
    );
    assert_eq!(decisions[0]["vp"], "no");
    assert_eq!(decisions[0]["owner"], "no");
    assert_eq!(decisions[1]["vp"], "rate");
    assert_eq!(decisions[1]["owner"], "global-cap");
    assert_eq!(decisions[2]["vp"], "below-min-severity");
    assert_eq!(decisions[2]["owner"], "below-min-severity");
    assert_eq!(decisions[3]["vp"], "hold");
    assert_eq!(
        decisions,
        vec![
            json!({"finding": findings[0], "vp": "no", "owner": "no", "streak": 1}),
            json!({"finding": findings[1], "vp": "rate", "owner": "global-cap", "streak": 1}),
            json!({"finding": findings[2], "vp": "below-min-severity", "owner": "below-min-severity", "streak": 1}),
            json!({"finding": findings[3], "vp": "hold", "owner": null, "streak": 1}),
        ]
    );
    assert_eq!(findings, original_findings);
    assert_eq!(state, original_state);
}

#[test]
fn owner_block_and_pending_confirmation_are_distinct_from_vp() {
    let config = config();
    let eligible = finding("host.disk_full", "root", "outage", "critical");
    let streaks = json!({"host.disk_full|root": {"count": 2}});
    let blocked_state = json!({"alerts": {}, "owner_pages": {}, "owner_page_times": []});
    let blocked = plan(
        &config,
        std::slice::from_ref(&eligible),
        &blocked_state,
        &streaks,
        1000.0,
        Some("state-loss"),
    );
    assert_eq!(blocked[0]["vp"], "no");
    assert_eq!(blocked[0]["owner"], "state-loss");

    let pending_state = json!({"alerts": {}, "owner_pages": {"host.disk_full|root": {"confirmed_at": 900.0}}, "owner_page_times": []});
    let pending = plan(&config, &[eligible], &pending_state, &streaks, 1000.0, None);
    assert_eq!(pending[0]["owner"], "dedupe");
}

#[test]
fn matches_actual_python_pure_function_oracle_cases() {
    let oracle: Value = serde_json::from_str(PYTHON_ORACLE).expect("embedded Python oracle JSON");
    assert_eq!(
        oracle["source"],
        "dunk_watchdog.py:1530-1623 exact AST pure functions and source constants"
    );
    let cases = oracle["cases"]
        .as_array()
        .expect("oracle cases are an array");
    assert_eq!(cases.len(), 28);

    for case in cases {
        let name = case["name"].as_str().expect("oracle case has a name");
        let function = case["function"]
            .as_str()
            .expect("oracle case has a function");
        let args = case["args"]
            .as_array()
            .expect("oracle arguments are an array");
        let original_args = args.clone();
        let actual = match function {
            "dedupe_key" => Value::String(dedupe_key(&args[0])),
            "advance_streaks" => advance_streaks(
                &args[0],
                &args[1],
                args[2].as_array().expect("findings are an array"),
                args[3].as_f64().expect("now is numeric"),
            ),
            "_cooling" => json!(cooling(
                &args[0],
                &args[1],
                &args[2],
                args[3].as_f64().expect("now is numeric"),
            )),
            "plan" => plan(
                &args[0],
                args[1].as_array().expect("findings are an array"),
                &args[2],
                &args[3],
                args[4].as_f64().expect("now is numeric"),
                args[5].as_str(),
            )
            .into(),
            other => panic!("unknown Python oracle function {other}"),
        };
        assert_eq!(actual, case["expected"], "Python oracle mismatch in {name}");
        assert_eq!(
            args.as_slice(),
            original_args.as_slice(),
            "inputs changed in {name}"
        );
        assert_eq!(
            case["unchanged"], true,
            "oracle did not verify purity in {name}"
        );
    }
}
