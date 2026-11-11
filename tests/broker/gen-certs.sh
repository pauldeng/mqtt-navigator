#!/usr/bin/env bash
# Makes test keys and certificates in tests/broker/certs. Deletes the old ones.
# These keys are for tests only. The folder is in .gitignore.
set -euo pipefail
cd "$(dirname "$0")"
rm -rf certs && mkdir certs
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/ca.cnf" <<CNF
[ca]
default_ca = d
[d]
database = $work/index.txt
new_certs_dir = $work
serial = $work/serial
default_md = sha256
policy = p
unique_subject = no
copy_extensions = copy
[p]
commonName = supplied
CNF
touch "$work/index.txt"
echo 01 > "$work/serial"

# A key (EC P-256) for each name.
for n in ca server expired wrong-host client client-mismatch; do
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "certs/$n.key"
done

openssl req -x509 -new -key certs/ca.key -sha256 -days 3650 -subj "/CN=Navigator Test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -out certs/ca.crt

# sign NAME SAN EKU START END
sign() {
  openssl req -new -key "certs/$1.key" -subj "/CN=$1" \
    -addext "subjectAltName=$2" -addext "extendedKeyUsage=$3" -out "$work/$1.csr"
  openssl ca -batch -notext -config "$work/ca.cnf" -cert certs/ca.crt -keyfile certs/ca.key \
    -startdate "$4" -enddate "$5" -in "$work/$1.csr" -out "certs/$1.crt" 2>/dev/null
}
now=$(date -u -d '-1 day' +%Y%m%d%H%M%SZ)
later=$(date -u -d '+10 years' +%Y%m%d%H%M%SZ)
local_san="DNS:localhost,IP:127.0.0.1"
sign server "$local_san" serverAuth "$now" "$later"
sign wrong-host "DNS:wrong.example" serverAuth "$now" "$later"
sign client "DNS:client" clientAuth "$now" "$later"
sign expired "$local_san" serverAuth 20200101000000Z 20200102000000Z
# client-mismatch.key does not match client.crt. The tests pair them on purpose.

# Password file for listener 18831: user "test", password "test-pass". Made in the broker image.
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/certs:/certs" --entrypoint mosquitto_passwd \
  "$(grep -m1 -o 'eclipse-mosquitto:[^ ]*' compose.yml)" -b -c /certs/passwd test test-pass
# The broker user must read the files. The keys are for tests only.
chmod 644 certs/*
