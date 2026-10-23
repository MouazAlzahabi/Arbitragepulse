#!/usr/bin/env bash
# ArbitragePulse — provision the engine host on AWS (Phase 1 of docs/hosting/AWS_SETUP.md).
#
#   scripts/aws/provision.sh --dry-run     # read-only lookups run; every create is only printed
#   scripts/aws/provision.sh               # create (asks before the paid steps; --yes skips asking)
#
# Creates, idempotently (re-running reuses what exists; everything is tagged
# Project=arbitragepulse):
#   key pair (your SSH public key) → security group (SSH from your IP only) →
#   t4g.small Ubuntu 24.04 arm64, 20 GB encrypted gp3, IMDSv2, standard CPU credits →
#   Elastic IP.
# The instance's user-data only installs packages, adds the `botadmin` user with your
# key, a 4 GB swapfile and chrony. It carries NO secrets: engine/base.env is written by
# hand on the server (Phase 3).
#
# Env overrides: AWS_PROFILE, AWS_REGION (us-east-1), INSTANCE_TYPE (t4g.small),
# SSH_PUBKEY (~/.ssh/id_ed25519.pub), SSH_CIDR (your current IP/32), NAME (arbitragepulse).
set -euo pipefail

DRY_RUN=false
YES=false
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --yes)     YES=true ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

REGION="${AWS_REGION:-us-east-1}"
TYPE="${INSTANCE_TYPE:-t4g.small}"
NAME="${NAME:-arbitragepulse}"
PUBKEY="${SSH_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"
TAG="ResourceType=%s,Tags=[{Key=Project,Value=arbitragepulse},{Key=Name,Value=$NAME}]"
AWS=(aws --region "$REGION")

command -v aws >/dev/null || { echo "aws CLI not found — brew install awscli" >&2; exit 1; }
[ -f "$PUBKEY" ] || { echo "SSH public key not found: $PUBKEY (set SSH_PUBKEY)" >&2; exit 1; }
case "$(head -c 8 "$PUBKEY")" in ssh-ed25|ssh-rsa*) ;; *) echo "$PUBKEY is not an SSH public key" >&2; exit 1 ;; esac

# Read-only calls always run (they find what already exists); creating calls go through
# `run`, which only prints them in --dry-run.
run() {
    if $DRY_RUN; then { printf '[dry-run]'; printf ' %q' "$@"; echo; } >&2; else "$@"; fi
}
confirm() {
    $DRY_RUN && return 0
    $YES && return 0
    read -r -p "$1 [y/N] " a; [ "$a" = y ] || [ "$a" = Y ] || { echo "Stopped."; exit 1; }
}

echo "Account: $("${AWS[@]}" sts get-caller-identity --query Account --output text)  Region: $REGION  Type: $TYPE"
SSH_CIDR="${SSH_CIDR:-$(curl -fsS https://checkip.amazonaws.com | tr -d '[:space:]')/32}"
echo "SSH allowed from: $SSH_CIDR"

# ── 1. Key pair ───────────────────────────────────────────────────────────────
if "${AWS[@]}" ec2 describe-key-pairs --key-names "$NAME" >/dev/null 2>&1; then
    echo "[1/5] key pair $NAME exists"
else
    echo "[1/5] importing key pair $NAME from $PUBKEY"
    run "${AWS[@]}" ec2 import-key-pair --key-name "$NAME" \
        --public-key-material "fileb://$PUBKEY" \
        --tag-specifications "$(printf "$TAG" key-pair)" >/dev/null
fi

# ── 2. Security group: inbound SSH from SSH_CIDR only ─────────────────────────
VPC=$("${AWS[@]}" ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
[ "$VPC" != "None" ] || { echo "no default VPC in $REGION — create one (VPC console → Actions → Create default VPC)" >&2; exit 1; }
SG=$("${AWS[@]}" ec2 describe-security-groups \
    --filters Name=group-name,Values="$NAME-sg" Name=vpc-id,Values="$VPC" \
    --query 'SecurityGroups[0].GroupId' --output text)
if [ "$SG" = "None" ]; then
    echo "[2/5] creating security group $NAME-sg in $VPC (SSH from $SSH_CIDR only)"
    if $DRY_RUN; then
        run "${AWS[@]}" ec2 create-security-group --group-name "$NAME-sg" --description "ArbitragePulse: SSH only" --vpc-id "$VPC"
        SG="sg-DRYRUN"
    else
        SG=$("${AWS[@]}" ec2 create-security-group --group-name "$NAME-sg" \
            --description "ArbitragePulse: SSH only" --vpc-id "$VPC" \
            --tag-specifications "$(printf "$TAG" security-group)" --query GroupId --output text)
    fi
    run "${AWS[@]}" ec2 authorize-security-group-ingress --group-id "$SG" \
        --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=$SSH_CIDR,Description=operator}]" >/dev/null
else
    echo "[2/5] security group $NAME-sg exists ($SG) — SSH rules:"
    "${AWS[@]}" ec2 describe-security-groups --group-ids "$SG" \
        --query 'SecurityGroups[0].IpPermissions[].[FromPort,ToPort,IpRanges[].CidrIp]' --output text
    echo "      (your IP changed? add it: aws ec2 authorize-security-group-ingress --group-id $SG --protocol tcp --port 22 --cidr <ip>/32)"
fi

# ── 3. Instance ───────────────────────────────────────────────────────────────
INST=$("${AWS[@]}" ec2 describe-instances \
    --filters Name=tag:Project,Values=arbitragepulse Name=tag:Name,Values="$NAME" \
              Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[0].Instances[0].InstanceId' --output text)
if [ "$INST" != "None" ]; then
    echo "[3/5] instance exists: $INST"
else
    AMI=$("${AWS[@]}" ssm get-parameter \
        --name /aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id \
        --query Parameter.Value --output text)
    echo "[3/5] launching $TYPE from $AMI (Ubuntu 24.04 arm64)"
    USERDATA=$(mktemp); trap 'rm -f "$USERDATA"' EXIT
    cat > "$USERDATA" <<EOF
#cloud-config
package_update: true
package_upgrade: true
packages: [build-essential, pkg-config, libssl-dev, git, curl, rsync, sqlite3, chrony, unattended-upgrades, python3]
users:
  - default
  - name: botadmin
    shell: /bin/bash
    ssh_authorized_keys:
      - $(cat "$PUBKEY")
write_files:
  # botadmin may only start/stop/restart/inspect the engine units (scripts/deploy.sh needs restart).
  - path: /etc/sudoers.d/arbitragepulse
    permissions: '0440'
    content: |
      botadmin ALL=(root) NOPASSWD: /usr/bin/systemctl restart arbitragepulse@*, /usr/bin/systemctl start arbitragepulse@*, /usr/bin/systemctl stop arbitragepulse@*, /usr/bin/systemctl status arbitragepulse@*
  - path: /etc/ssh/sshd_config.d/99-arbitragepulse.conf
    content: |
      PasswordAuthentication no
      PermitRootLogin no
runcmd:
  - [sh, -c, 'fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile && echo "/swapfile none swap sw 0 0" >> /etc/fstab']
  - [systemctl, enable, --now, chrony]
  - [systemctl, restart, ssh]
EOF
    COST="t4g.small ≈ \$12.3/mo + public IPv4 ≈ \$3.7/mo + 20 GB gp3 ≈ \$1.6/mo"
    [ "$TYPE" = t4g.small ] || COST="see https://aws.amazon.com/ec2/pricing/on-demand/ for $TYPE"
    confirm "Launch $TYPE now? Billing starts ($COST)."
    if $DRY_RUN; then
        run "${AWS[@]}" ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name "$NAME" \
            --security-group-ids "$SG" --user-data "file://<cloud-config: packages, botadmin, swap, chrony>" \
            --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=20,VolumeType=gp3,Encrypted=true,DeleteOnTermination=true}' \
            --credit-specification CpuCredits=standard \
            --metadata-options HttpTokens=required,HttpEndpoint=enabled
        INST="i-DRYRUN"
    else
        INST=$("${AWS[@]}" ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name "$NAME" \
            --security-group-ids "$SG" --user-data "file://$USERDATA" \
            --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=20,VolumeType=gp3,Encrypted=true,DeleteOnTermination=true}' \
            --credit-specification CpuCredits=standard \
            --metadata-options HttpTokens=required,HttpEndpoint=enabled \
            --tag-specifications "$(printf "$TAG" instance)" "$(printf "$TAG" volume)" \
            --query 'Instances[0].InstanceId' --output text)
        echo "      waiting for $INST to run…"
        "${AWS[@]}" ec2 wait instance-running --instance-ids "$INST"
    fi
fi

# ── 4. Elastic IP ─────────────────────────────────────────────────────────────
EIP=$("${AWS[@]}" ec2 describe-addresses --filters Name=tag:Project,Values=arbitragepulse \
    --query 'Addresses[0].[AllocationId,PublicIp,InstanceId]' --output text)
if [ "$EIP" = "None" ] || [ -z "$EIP" ]; then
    confirm "Allocate an Elastic IP (≈ \$3.7/mo, billed even while the instance is stopped)?"
    echo "[4/5] allocating Elastic IP"
    if $DRY_RUN; then
        run "${AWS[@]}" ec2 allocate-address --domain vpc
        ALLOC="eipalloc-DRYRUN"; IP="<eip>"
    else
        read -r ALLOC IP < <("${AWS[@]}" ec2 allocate-address --domain vpc \
            --tag-specifications "$(printf "$TAG" elastic-ip)" --query '[AllocationId,PublicIp]' --output text)
    fi
    ATTACHED="None"
else
    read -r ALLOC IP ATTACHED <<<"$EIP"
    echo "[4/5] Elastic IP exists: $IP"
fi
if [ "$ATTACHED" != "$INST" ]; then
    echo "      associating $IP with $INST"
    run "${AWS[@]}" ec2 associate-address --allocation-id "$ALLOC" --instance-id "$INST" >/dev/null
fi

# ── 5. Summary ────────────────────────────────────────────────────────────────
echo "[5/5] done"
echo ""
echo "  Instance:  $INST ($TYPE, $REGION)"
echo "  Public IP: $IP"
echo "  SSH:       ssh botadmin@$IP        (admin with sudo: ssh ubuntu@$IP)"
echo "  Cloud-init needs ~3 min on first boot (packages + swap): ssh ubuntu@$IP cloud-init status --wait"
echo "  Next:      Phase 2 in docs/hosting/AWS_SETUP.md"
