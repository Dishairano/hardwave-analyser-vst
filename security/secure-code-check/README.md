# Secure Code Check keys

Every release carries a Secure Code Check report signed by our build. The files here let you
check that signature with OpenSSH, without trusting our website.

- `signing_key.pub`: the public key our build signs reports with since 13 Nov 2026.
  Fingerprint `SHA256:g7eOKThwBAjeX7I5sCDrBwFZSi5gt2weXiNQNRtJ+mg`.
- `allowed_signers`: the same key, in the format `ssh-keygen -Y verify -f` reads.
- `revoked_signers`: keys we no longer sign with. Pass it with `-r` so a signature by one of
  them fails.

```
ssh-keygen -Y verify -f allowed_signers -r revoked_signers \
  -I secure-code-check@hardwavestudios.com -n hardwave-secure-code-check \
  -s REPORT.json.sig < REPORT.json
```

## Revoked key, 13 Nov 2026

The previous key, fingerprint `SHA256:b8cIv1uoJ0oH292J/WpdKgqBJuVmVOXrvSCdXHICpZM`, leaked from
our build system in a security incident and is revoked as of 13 Nov 2026.

- A report signed with it before 13 Nov 2026 stays valid only if it was published before
  10 Nov 2026.
- Anything else signed with it, whatever date it carries, is not proof that it came from us.
- From 13 Nov 2026 on, reports are signed only with the key above.
