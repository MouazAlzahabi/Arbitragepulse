#!/usr/bin/env bash
# ArbitragePulse — delete everything provision.sh created (tag Project=arbitragepulse).
#
#   scripts/aws/teardown.sh --dry-run   # list what would be deleted
#   scripts/aws/teardown.sh             # delete (asks first)
#
# Terminating the instance deletes its disk, including trades.db — copy backups off first
# (docs/hosting/AWS_SETUP.md, "Backups"). Withdraw funds from the executor contract before
# tearing down a live host; the contract itself is on-chain and unaffected.
set -euo pipefail

DRY_RUN=false
[ "${1:-}" = "--dry-run" ] && DRY_RUN=true
REGION="${AWS_REGION:-us-east-1}"
NAME="${NAME:-arbitragepulse}"
AWS=(aws --region "$REGION")
run() { if $DRY_RUN; then { printf '[dry-run]'; printf ' %q' "$@"; echo; } >&2; else "$@"; fi; }

INSTS=$("${AWS[@]}" ec2 describe-instances \
    --filters Name=tag:Project,Values=arbitragepulse Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text)
ALLOCS=$("${AWS[@]}" ec2 describe-addresses --filters Name=tag:Project,Values=arbitragepulse \
    --query 'Addresses[].AllocationId' --output text)
SG=$("${AWS[@]}" ec2 describe-security-groups --filters Name=group-name,Values="$NAME-sg" \
    --query 'SecurityGroups[0].GroupId' --output text)

echo "Region $REGION — will delete:"
echo "  instances:      ${INSTS:-none}"
echo "  elastic IPs:    ${ALLOCS:-none}"
echo "  security group: $SG"
echo "  key pair:       $NAME"
if ! $DRY_RUN; then
    read -r -p "Delete all of the above? This destroys the server disk. Type 'delete': " a
    [ "$a" = delete ] || { echo "Stopped."; exit 1; }
fi

for a in $ALLOCS; do
    ASSOC=$("${AWS[@]}" ec2 describe-addresses --allocation-ids "$a" --query 'Addresses[0].AssociationId' --output text)
    [ "$ASSOC" = "None" ] || run "${AWS[@]}" ec2 disassociate-address --association-id "$ASSOC"
    run "${AWS[@]}" ec2 release-address --allocation-id "$a"
done
if [ -n "$INSTS" ]; then
    # shellcheck disable=SC2086
    run "${AWS[@]}" ec2 terminate-instances --instance-ids $INSTS >/dev/null
    # shellcheck disable=SC2086
    $DRY_RUN || "${AWS[@]}" ec2 wait instance-terminated --instance-ids $INSTS
fi
[ "$SG" = "None" ] || run "${AWS[@]}" ec2 delete-security-group --group-id "$SG"
run "${AWS[@]}" ec2 delete-key-pair --key-name "$NAME"
echo "Done."
