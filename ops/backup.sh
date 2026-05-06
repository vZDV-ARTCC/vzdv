#!/bin/bash
set -e

echo "Creating backup"
cd /home/zdv/vzdv
sqlite3 vzdv_data.sqlite ".backup vzdv_data.sqlite.backup"
b2v3 file upload zdv-wm-mn-files vzdv_data.sqlite.backup vzdv_data.sqlite
rm vzdv_data.sqlite.backup
echo "Done"
