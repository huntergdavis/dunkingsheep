# Recovery metadata equality fixture

`metadata_equality_python.json` records the 42 original JSON token strings and
their 1,764 pairwise equality results, captured with Python 3.14.4 using the
`json.loads` and `a == b` expression from the test at Git commit
`5c9839aa286dcd070bb938e60e368ae7f126bf05`. The test checks the token list against
the fixture before replaying the matrix, and retains the no-rewrite checks for
equal values. This snapshot does not track future Python behavior automatically.
Other recovery tests still execute the archived Python oracle.
