#!/bin/sh
# Only on removal: dpkg passes "remove", rpm passes 0. An upgrade passes
# something else and must leave the service running.
case "$1" in
    remove|0) systemctl disable --now netmeterd.service || true ;;
esac
