# Deploying PMos on S3 + CloudFront

How to host PMos on AWS using S3 as the origin and CloudFront as the HTTPS edge, with a CloudFront Response Headers Policy injecting the cross-origin-isolation headers the kernel's syscall transport requires.

## Overview

S3 stores the static `dist/` tree; CloudFront sits in front of it to serve HTTPS at the edge and — the point of this recipe — attach `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` to every response via a **Response Headers Policy** bound to the distribution's default cache behaviour. This replaces what used to require a Lambda@Edge function or a CloudFront Function: the managed policy is a declarative JSON document, ships with zero code, and cannot drift, error out, or exceed an execution budget. See [`../.specify/memory/constitution.md`](../.specify/memory/constitution.md) Principle IV (Offline-First And Persistent) for the upstream constraint — without `window.crossOriginIsolated === true`, `SharedArrayBuffer` and `Atomics.wait` are gated off and the kernel Worker cannot talk to app Workers.

## Prerequisites

- An AWS account with permission to create S3 buckets, CloudFront
  distributions, and (optionally) Route 53 records.
- AWS CLI v2 installed and configured. Verify with
  `aws sts get-caller-identity` — it must print an account and ARN
  without prompting.
- A domain managed in Route 53 (optional but recommended; needed for
  ACM-issued TLS and a human-friendly hostname). Without it you will
  use the auto-generated `*.cloudfront.net` domain.
- PMos build artefacts in `dist/` — i.e. you have run `just build`
  and `dist/index.html` + `dist/assets/` exist on disk.

## Deploy steps

Export placeholder variables at the top of your shell session so the
rest of the commands paste verbatim:

```shell
$ export BUCKET_NAME=my-pmos-bucket
$ export DISTRIBUTION_ID=          # filled in after step 5
$ export POLICY_ID=                # filled in after step 4
```

1. **Create the S3 bucket.** Bucket names are globally unique, so
   pick something distinctive. This recipe uses `us-east-1` because
   CloudFront's default-certificate flow and several older APIs
   expect it; you can pick another region if you are providing your
   own ACM cert in `us-east-1`.

   ```shell
   $ aws s3api create-bucket --bucket $BUCKET_NAME --region us-east-1
   ```

2. **Enable static website hosting on the bucket.** This gives the
   bucket an `index.html` default and a sensible 404 page. Even
   though CloudFront will be the public entry point, keeping the
   website configuration on the bucket makes direct origin tests
   easier.

   ```shell
   $ aws s3 website s3://$BUCKET_NAME --index-document index.html
   ```

3. **Upload `dist/`.** `--delete` makes the sync idempotent: files
   removed from the local build will be removed from the bucket on
   the next run.

   ```shell
   $ aws s3 sync ./dist s3://$BUCKET_NAME --delete
   ```

4. **Create the Response Headers Policy.** Save the JSON from
   [§ The Response Headers Policy JSON](#the-response-headers-policy-json)
   below as `coop-coep-policy.json`, then:

   ```shell
   $ aws cloudfront create-response-headers-policy \
         --response-headers-policy-config file://coop-coep-policy.json \
         --query 'ResponseHeadersPolicy.Id' --output text
   ```

   Capture the printed ID into the `POLICY_ID` variable you exported
   above: `export POLICY_ID=<id-from-output>`.

5. **Create the CloudFront distribution.** Save the skeleton below
   as `distribution-config.json`, substituting `$POLICY_ID` and
   `$BUCKET_NAME` (shell variables do not expand inside a JSON file,
   so either edit the file by hand or generate it with `envsubst`):

   ```json
   {
     "CallerReference": "pmos-dist-1",
     "Comment": "PMos static OS distribution",
     "Enabled": true,
     "DefaultRootObject": "index.html",
     "Origins": {
       "Quantity": 1,
       "Items": [
         {
           "Id": "pmos-s3-origin",
           "DomainName": "my-pmos-bucket.s3.us-east-1.amazonaws.com",
           "S3OriginConfig": { "OriginAccessIdentity": "" }
         }
       ]
     },
     "DefaultCacheBehavior": {
       "TargetOriginId": "pmos-s3-origin",
       "ViewerProtocolPolicy": "redirect-to-https",
       "CachePolicyId": "658327ea-f89d-4fab-a63d-7e88639e58f6",
       "ResponseHeadersPolicyId": "REPLACE_WITH_POLICY_ID_FROM_STEP_4",
       "Compress": true,
       "AllowedMethods": {
         "Quantity": 2,
         "Items": ["GET", "HEAD"],
         "CachedMethods": { "Quantity": 2, "Items": ["GET", "HEAD"] }
       }
     },
     "PriceClass": "PriceClass_100"
   }
   ```

   The `CachePolicyId` above is AWS's managed **CachingOptimized**
   policy, stable across accounts. `ResponseHeadersPolicyId` is the
   load-bearing field for this recipe — it is what wires the COOP/COEP
   policy from step 4 into every response.

   Then create the distribution:

   ```shell
   $ aws cloudfront create-distribution \
         --distribution-config file://distribution-config.json \
         --query 'Distribution.Id' --output text
   ```

   Capture the printed ID into `DISTRIBUTION_ID`. The distribution
   takes 5–15 minutes to deploy to every edge; `aws cloudfront
   wait distribution-deployed --id $DISTRIBUTION_ID` will block
   until it is ready.

6. **(Optional) Add a Route 53 alias.** If you own a domain in
   Route 53 and have provisioned an ACM certificate in `us-east-1`
   covering it (then attached via `ViewerCertificate` on the
   distribution — not shown in the skeleton above), add an A-record
   alias pointing the hostname at the CloudFront domain. In the
   AWS console: **Route 53 → Hosted zones → your zone → Create
   record → Alias → Route traffic to → Alias to CloudFront
   distribution → pick `$DISTRIBUTION_ID`**.

7. **Smoke-test.** Use the CloudFront domain (or your Route 53
   hostname) to confirm the headers land:

   ```shell
   $ curl -sI https://<distribution-domain>.cloudfront.net/ \
         | grep -E '^cross-origin-'
   ```

   Then load the same URL in a browser, open DevTools, and in the
   Console run:

   ```js
   console.log(window.crossOriginIsolated)  // must print: true
   ```

## The Response Headers Policy JSON

The two headers PMos actually needs — `Cross-Origin-Opener-Policy`
and `Cross-Origin-Embedder-Policy` — are both first-class fields in
`SecurityHeadersConfig`, so no `CustomHeadersConfig` block is
needed. `"Override": true` tells CloudFront to overwrite any value
the origin sent; S3 sends neither header, but the override makes
this policy safe to reuse on any origin without first auditing what
the origin emits.

Save as `coop-coep-policy.json`:

```json
{
  "Name": "pmos-coop-coep",
  "Comment": "Attach COOP/COEP to every response so window.crossOriginIsolated === true in the browser; required by PMos kernel (Principle IV).",
  "SecurityHeadersConfig": {
    "CrossOriginOpenerPolicy": {
      "Override": true,
      "CrossOriginOpenerPolicy": "same-origin"
    },
    "CrossOriginEmbedderPolicy": {
      "Override": true,
      "CrossOriginEmbedderPolicy": "require-corp"
    }
  }
}
```

## Verifying the deploy

```shell
$ curl -sI https://<distribution-domain>.cloudfront.net/ \
      | grep -E '^cross-origin-(opener|embedder)-policy'
cross-origin-opener-policy: same-origin
cross-origin-embedder-policy: require-corp
```

Both lines must appear exactly once. Then, in the browser DevTools
console on the deployed page:

```js
console.log(window.crossOriginIsolated)  // true
```

If both checks pass, PMos will boot.

## Troubleshooting

### `crossOriginIsolated` is false after deploy

Almost always caused by a sub-resource (image, script, font, iframe)
served from a third-party origin without an opt-in header — the same
failure mode documented in [`deploy-github-pages.md`](./deploy-github-pages.md).
Under `require-corp`, every cross-origin sub-resource must itself
send either `Cross-Origin-Resource-Policy: cross-origin` or a
matching CORS policy.

Fix:

1. Open DevTools **Network** tab, filter by "blocked", and identify
   the offending origins.
2. If the resource is one you control, add CORP on its response.
3. If the resource is proxied through CloudFront, extend the
   Response Headers Policy with a `CustomHeadersConfig` entry
   setting `Cross-Origin-Resource-Policy: cross-origin` on its
   path, or put that origin behind its own cache behaviour.
4. If the resource is third-party and uncontrollable, remove it.
   PMos itself loads nothing from outside the deployment origin by
   design, so the fault is always in customisations you added.

### CloudFront cache serves old headers after a policy change

CloudFront caches responses at the edge, so updating the Response
Headers Policy does not retroactively rewrite in-flight cache
entries. Invalidate:

```shell
$ aws cloudfront create-invalidation \
      --distribution-id $DISTRIBUTION_ID --paths "/*"
```

Wait about 60 seconds and re-run the `curl -sI` check.

### S3 bucket returns 403 for direct access

If you attached an **Origin Access Control** (OAC) to the
distribution, direct S3 access from outside CloudFront is blocked
on purpose — that is the whole point of an OAC, and you should
leave it that way. The bucket is reachable only via the CloudFront
URL, which is correct.

If you see 403 via the **CloudFront** URL itself (not direct S3),
the bucket policy is missing the OAC principal. The OAC setup wizard
in the CloudFront console offers to write this policy for you; if
you skipped that step, add a statement like the following to the
bucket policy, substituting your account ID and distribution ARN:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "AllowCloudFrontServicePrincipal",
      "Effect": "Allow",
      "Principal": { "Service": "cloudfront.amazonaws.com" },
      "Action": "s3:GetObject",
      "Resource": "arn:aws:s3:::my-pmos-bucket/*",
      "Condition": {
        "StringEquals": {
          "AWS:SourceArn": "arn:aws:cloudfront::111122223333:distribution/EXAMPLEDIST"
        }
      }
    }
  ]
}
```
