#!/bin/sh
# .deb and .rpm: netmeterd does the recording, so it is enabled on install
# (packaging/netmeter.install does the same on Arch).
systemctl daemon-reload || true
systemctl enable netmeterd.service || true
# restart, not start: on an upgrade the old binary would otherwise keep running.
systemctl restart netmeterd.service || true
