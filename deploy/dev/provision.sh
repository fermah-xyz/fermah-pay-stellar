#!/bin/sh
# Provisions one product, a testnet deployment bound to the ledger contract
# recorded in the testnet profile, and an API key, once: the result is kept
# in /state and reused on every later start.
set -eu

if [ -f /state/seller.json ]; then
    echo "already provisioned:"
    jq '{deployment_id, contract}' /state/seller.json
    exit 0
fi

admin() { fermah-pay-stellar-admin --database-url "$ISSUER_DATABASE_URL" "$@"; }
deployment=/profile/deployment.json
contract=$(jq -r .contract "$deployment")
treasury=$(jq -r .treasury "$deployment")
operator=$(jq -r .operator "$deployment")

product=$(admin create-product --name dev | jq -r .product_id)
deployment_id=$(admin create-deployment --product-id "$product" --name testnet \
    --network stellar:testnet | jq -r .deployment_id)
admin bind-ledger --deployment-id "$deployment_id" --contract "$contract" \
    --treasury "$treasury" --operator "$operator" > /dev/null
key=$(admin issue-api-key --deployment-id "$deployment_id" --label dev)

umask 077
jq -n --arg deployment_id "$deployment_id" --arg contract "$contract" \
    --argjson key "$key" \
    '{deployment_id: $deployment_id, contract: $contract, api_key: $key.token}' \
    > /state/seller.json
echo "provisioned deployment $deployment_id; the API key is in the state volume"
