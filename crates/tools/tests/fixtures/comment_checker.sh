#!/bin/sh
# Test-only wrapper, replaced with the absolute fixture path by the test harness.
# exec keeps the CLI leader PID identical to the supervisor's direct child.
exec '__COMMENT_CHECKER_TEST_FIXTURE__' "$@"
