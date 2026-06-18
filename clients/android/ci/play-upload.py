#!/usr/bin/env python3
"""Upload a signed AAB to Google Play via the Publishing API — a direct replacement for
r0adkll/upload-google-play, which swallows real API errors into "Unknown error occurred."

Why hand-rolled: stdlib + `openssl` only (no pip on the runner), and it prints Google's actual
error at the stage it fails instead of a catch-all. Reuses the SERVICE_ACCOUNT_JSON secret and
tolerates it being raw JSON *or* base64-encoded JSON.

Usage:
  SERVICE_ACCOUNT_JSON='<raw-or-base64 SA key>' \
    python3 play-upload.py --package io.unom.punktfunk \
      --aab path/to/app-release.aab --track internal --status completed [--no-commit]

--no-commit: do insert -> upload -> track-update -> validate, then delete the edit (publishes
nothing). Use it to dry-run the credentials/AAB without touching the live track.
"""
import argparse, base64, json, os, subprocess, sys, tempfile, time
import urllib.request, urllib.parse, urllib.error

API = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications"
UPLOAD = "https://androidpublisher.googleapis.com/upload/androidpublisher/v3/applications"


class ApiError(Exception):
    def __init__(self, code, method, url, body):
        super().__init__(f"HTTP {code} from {method} {url}\n{body}")
        self.code, self.body = code, body


def _b64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def call(method, url, token=None, data=None, content_type=None, want_json=True):
    headers = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if content_type:
        headers["Content-Type"] = content_type
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            body = r.read()
    except urllib.error.HTTPError as e:
        raise ApiError(e.code, method, url, e.read().decode("utf-8", "replace"))
    return json.loads(body) if (want_json and body) else body


def load_sa():
    raw = os.environ.get("SERVICE_ACCOUNT_JSON", "")
    if not raw.strip():
        sys.exit("ERROR: SERVICE_ACCOUNT_JSON env is empty")
    try:                                        # raw JSON (what r0adkll expects)
        return json.loads(raw)
    except json.JSONDecodeError:
        try:                                    # or base64-encoded JSON (common mistake)
            sa = json.loads(base64.b64decode(raw))
            print("note: SERVICE_ACCOUNT_JSON was base64-encoded; decoded it.")
            return sa
        except Exception:
            sys.exit("ERROR: SERVICE_ACCOUNT_JSON is neither valid JSON nor base64-encoded JSON")


def access_token(sa) -> str:
    now = int(time.time())
    header = _b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    claims = _b64url(json.dumps({
        "iss": sa["client_email"],
        "scope": "https://www.googleapis.com/auth/androidpublisher",
        "aud": sa["token_uri"], "iat": now, "exp": now + 3600,
    }).encode())
    signing_input = f"{header}.{claims}".encode()
    with tempfile.NamedTemporaryFile("w", suffix=".pem", delete=False) as f:
        f.write(sa["private_key"])
        keyfile = f.name
    try:
        sig = subprocess.run(["openssl", "dgst", "-sha256", "-sign", keyfile],
                             input=signing_input, capture_output=True, check=True).stdout
    finally:
        os.unlink(keyfile)
    jwt = f"{header}.{claims}.{_b64url(sig)}"
    tok = call("POST", sa["token_uri"],
               data=urllib.parse.urlencode({
                   "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
                   "assertion": jwt}).encode(),
               content_type="application/x-www-form-urlencoded")
    return tok["access_token"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--package", required=True)
    ap.add_argument("--aab", required=True)
    ap.add_argument("--track", default="internal")
    ap.add_argument("--status", default="completed")
    ap.add_argument("--no-commit", action="store_true")
    a = ap.parse_args()

    if not os.path.isfile(a.aab):
        sys.exit(f"ERROR: AAB not found: {a.aab}")

    sa = load_sa()
    tok = access_token(sa)
    print(f"authenticated as {sa['client_email']} (project {sa.get('project_id')})")
    app = f"{API}/{a.package}"

    try:
        edit = call("POST", f"{app}/edits", token=tok)["id"]
        with open(a.aab, "rb") as f:
            blob = f.read()
        print(f"uploading {a.aab} ({len(blob)} bytes) ...")
        vc = call("POST", f"{UPLOAD}/{a.package}/edits/{edit}/bundles?uploadType=media",
                  token=tok, data=blob, content_type="application/octet-stream")["versionCode"]
        print(f"uploaded versionCode={vc}")
        call("PUT", f"{app}/edits/{edit}/tracks/{a.track}", token=tok,
             data=json.dumps({"track": a.track,
                              "releases": [{"status": a.status, "versionCodes": [str(vc)]}]}).encode(),
             content_type="application/json")
        print(f"assigned versionCode={vc} -> track={a.track} status={a.status}")

        if a.no_commit:
            call("POST", f"{app}/edits/{edit}:validate", token=tok)
            print("validated (dry-run) OK — deleting edit, nothing published")
            call("DELETE", f"{app}/edits/{edit}", token=tok, want_json=False)
            return

        try:
            call("POST", f"{app}/edits/{edit}:commit", token=tok)
        except ApiError as e:
            if "changesNotSentForReview" in e.body:
                print("commit needs changesNotSentForReview=true — retrying")
                call("POST", f"{app}/edits/{edit}:commit?changesNotSentForReview=true", token=tok)
            else:
                raise
        print(f"COMMITTED: versionCode={vc} live on track '{a.track}' ({a.status})")
    except ApiError as e:
        print(f"\nPLAY API ERROR:\n{e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
