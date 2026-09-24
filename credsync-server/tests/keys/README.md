# Test keys

A throwaway RSA-2048 keypair, generated for this repository and committed on purpose.

It exists for one test: `an_hs256_token_signed_with_the_rsa_public_key_is_refused`, which
reproduces the classic RS256 → HS256 algorithm-confusion attack. That attack needs a real
asymmetric keypair, because its whole mechanism is signing an HMAC with the *public* key bytes —
the ones an attacker legitimately has.

**This key signs nothing real.** It is not used by `credsyncd`, it is not a default, and it is not
referenced outside `tests/`. Committing it is safe in the way a test fixture is safe, and it makes
a security property reproducible rather than aspirational.

Regenerate with:

```sh
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out test-rsa-private.pem
openssl rsa -in test-rsa-private.pem -pubout -out test-rsa-public.pem
```
