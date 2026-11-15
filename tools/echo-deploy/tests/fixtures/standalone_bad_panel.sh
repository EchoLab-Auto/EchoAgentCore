#!/bin/sh
# TEST STUB ONLY — deliberately not the real Panel executable.
#
# The release manifest declares this file's bytes as the executable Panel
# artifact. The process that actually runs is /bin/sh then `sleep`, so a
# running-process image check must reject it instead of trusting
# ActiveState=active. It must never be installed or started outside
# tests/standalone_live.rs.
echo "standalone-live bad-panel stub: refusing to serve" >&2
exec sleep 300
