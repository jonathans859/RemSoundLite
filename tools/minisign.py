"""Minisign-compatible key generation and signing, for release builds.

The updater (ship-shape) checks every downloaded release against the public key compiled
into the app. This makes that key and signs release archives with Python's `cryptography`
package, so CI needs no minisign binary.

    python tools/minisign.py keygen SECRET_KEY_FILE      prints the public key
    python tools/minisign.py sign FILE                   writes FILE.minisig

`sign` reads the secret key (the whole key file) from the MINISIGN_SECRET_KEY environment
variable. Keys are unencrypted, like `minisign -G -W`, since CI can't type a password.
Signatures use minisign's pre-hashed "ED" algorithm (BLAKE2b-512 of the file).
"""

import base64
import hashlib
import os
import secrets
import sys
import time

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def keygen(path):
    private = Ed25519PrivateKey.generate()
    seed = private.private_bytes(serialization.Encoding.Raw, serialization.PrivateFormat.Raw,
                                 serialization.NoEncryption())
    public = private.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    key_id = secrets.token_bytes(8)
    secret = seed + public
    checksum = hashlib.blake2b(b"Ed" + key_id + secret, digest_size=32).digest()
    # sig alg, kdf alg (none), checksum alg, kdf salt, opslimit, memlimit, key id, key, checksum
    blob = b"Ed" + b"\0\0" + b"B2" + secrets.token_bytes(32) + bytes(16) + key_id + secret + checksum
    with open(path, "w", newline="\n") as f:
        f.write("untrusted comment: minisign secret key for RemSoundLite releases\n")
        f.write(base64.b64encode(blob).decode() + "\n")
    print(base64.b64encode(b"Ed" + key_id + public).decode())


def load_secret(text):
    blob = base64.b64decode(text.strip().splitlines()[-1])
    if blob[:2] != b"Ed" or blob[2:4] != b"\0\0":
        sys.exit("not an unencrypted minisign secret key")
    key_id = blob[54:62]
    secret = blob[62:126]
    checksum = blob[126:158]
    if hashlib.blake2b(b"Ed" + key_id + secret, digest_size=32).digest() != checksum:
        sys.exit("secret key checksum mismatch")
    return key_id, Ed25519PrivateKey.from_private_bytes(secret[:32])


def sign(path):
    text = os.environ.get("MINISIGN_SECRET_KEY")
    if not text:
        sys.exit("MINISIGN_SECRET_KEY is not set")
    key_id, private = load_secret(text)
    with open(path, "rb") as f:
        digest = hashlib.blake2b(f.read(), digest_size=64).digest()
    signature = private.sign(digest)
    trusted = f"timestamp:{int(time.time())}\tfile:{os.path.basename(path)}\thashed"
    global_signature = private.sign(signature + trusted.encode())
    with open(path + ".minisig", "w", newline="\n") as f:
        f.write("untrusted comment: signature from RemSoundLite release key\n")
        f.write(base64.b64encode(b"ED" + key_id + signature).decode() + "\n")
        f.write(f"trusted comment: {trusted}\n")
        f.write(base64.b64encode(global_signature).decode() + "\n")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "keygen":
        keygen(sys.argv[2])
    elif len(sys.argv) == 3 and sys.argv[1] == "sign":
        sign(sys.argv[2])
    else:
        sys.exit(__doc__)
