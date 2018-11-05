#!/bin/sh
if [ "$#" -eq 2 ]
then
	openssl rand -hex 32
	hexdump -n 32 -e '8 "%08x" 1 "\n"' /dev/random
	export TOX_SECRET_KEY=`od -vN 32 -An -tx1 /dev/random | tr -d " \n"`

    ./tox-node --bootstrap-node $1 $2 --udp-address '0.0.0.0:33445' --tcp-address '0.0.0.0:33445' --no-lan --motd 'This is tox-rs.'
else
	echo "Arguments are bootstrap PublicKey, bootstrap IP:Port"
	exit 1
fi
