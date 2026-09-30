# Deploying on Google Cloud's free tier

> **Live since 2026-09-29:** project `pf-ai-keeper`, VM `aikeeper` (e2-micro, us-central1-a), service
> account `aikeeper@pf-ai-keeper.iam.gserviceaccount.com` with `objectViewer` on `gs://pfkeeperdata`
> only, $1 budget alert. The VM installs itself with `deploy/gce-startup.sh` (below).
>
> **Email:** Cloud console → Compute Engine → VM instances → `aikeeper` → Edit → Metadata → add
> `aik-smtp-password` (the Gmail app password) → Save → Reset the VM. The startup script copies it
> into `/etc/aikeeper.env` at every boot, so it never goes through chat or git.

Target: one **e2-micro** VM in `us-central1` (Iowa), a separate Google Cloud project, a service
account that can only **read** the keeper's bucket. About 20 minutes, mostly copy-paste.

## What it costs

| Item | Cost |
|---|---|
| e2-micro VM + 30 GB standard disk, us-central1 | always-free tier (1 per billing account) |
| Reading `gs://pfkeeperdata` | the bucket is **US multi-region**, which the free Storage allowance does not cover (it covers us-east1/us-west1/us-central1 only). At $0.0004 per 1,000 reads: about **$0.001 a month** live and **~$0.01 once** for the history backfill. Transfer US → us-central1 is free. |
| VM external IPv4 address | **Not confirmed.** Google's free-tier page does not say either way; if charged it is about $0.005/hour. The $1 budget alert below will tell you within days. |
| Coinbase, Deribit, Base public RPC, Gmail | free |

Set the budget alert first (step 2) so any surprise shows up as an email, not a bill.

## 1. Project

In Cloud Shell (console.cloud.google.com → the `>_` icon) — it already has `gcloud`:

```bash
PROJECT=pf-ai-keeper            # pick any free ID
gcloud projects create $PROJECT --name="Panda AI Keeper"
gcloud billing accounts list    # copy the ACCOUNT_ID of your existing billing account
gcloud billing projects link $PROJECT --billing-account=ACCOUNT_ID
gcloud config set project $PROJECT
gcloud services enable compute.googleapis.com
```

## 2. Budget alert ($1)

Console → Billing → Budgets & alerts → Create budget → Projects: `pf-ai-keeper`, amount **$1**,
alerts at 50% / 100% to your email.

## 3. A service account that can only read the keeper's bucket

```bash
gcloud iam service-accounts create aikeeper --display-name="A.I Keeper (read-only)"
SA=aikeeper@$PROJECT.iam.gserviceaccount.com
# The ONLY permission it gets: list + read objects in the keeper's history bucket.
gcloud storage buckets add-iam-policy-binding gs://pfkeeperdata \
  --member=serviceAccount:$SA --role=roles/storage.objectViewer
```

Do **not** give it any project role, and do **not** reuse the keeper's `HISTORY_GCS_KEY_JSON` (that
key can create objects).

## 4. The VM

```bash
gcloud compute instances create aikeeper \
  --zone=us-central1-a --machine-type=e2-micro \
  --image-family=debian-12 --image-project=debian-cloud \
  --boot-disk-size=30GB --boot-disk-type=pd-standard \
  --service-account=$SA --scopes=https://www.googleapis.com/auth/devstorage.read_only
```

The `devstorage.read_only` scope is a second lock on top of the IAM role: even a bug could not use
the VM's identity to write to storage.

## 5. Install

```bash
gcloud compute ssh aikeeper --zone=us-central1-a
# on the VM:
sudo apt-get update && sudo apt-get install -y git build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source ~/.cargo/env
# 1 GB of RAM is tight for compiling: add 2 GB of swap first (stays on the free 30 GB disk).
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile && sudo swapon /swapfile
git clone https://github.com/Biosupra/A.I-Keeper.git && cd A.I-Keeper
cargo build --release          # ~15-25 minutes on an e2-micro
sudo ./deploy/install.sh       # installs the binary, the service and /etc/aikeeper.env
```

## 6. Email (Gmail app password)

On your Google account: Security → 2-Step Verification (must be on) → App passwords → create one
named "A.I Keeper". Then on the VM:

```bash
sudo nano /etc/aikeeper.env      # fill AIK_SMTP_USER, AIK_SMTP_PASSWORD, AIK_EMAIL_TO
```

The app password can only send mail as you; it cannot touch any wallet. Revoke it any time from the
same page.

## 7. Start

```bash
sudo -u aikeeper /usr/local/bin/aikeeper-env selftest    # every source + the guardrails
sudo -u aikeeper /usr/local/bin/aikeeper-env backfill --days 365   # ~30-45 min, once
sudo systemctl enable --now aikeeper
journalctl -u aikeeper -f        # watch it work
```

The first brief arrives at the next `AIK_BRIEF_HOUR_UTC` (default 12:00 UTC = 8 am Eastern).
Shadow decisions are graded 24 hours after they are made; the scorecard fills in over the first days.

## Stop / remove

`sudo systemctl stop aikeeper` stops everything. Deleting the VM (or the whole `pf-ai-keeper`
project) removes it completely; the keeper, the website and the bucket are unaffected.
