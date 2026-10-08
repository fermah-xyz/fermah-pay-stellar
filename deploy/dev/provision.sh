#!/bin/sh
# Provisions one product, a testnet deployment bound to the ledger contract
# recorded in the testnet profile, and an API key, once: the result is kept
# in /state and reused on every later start. When the profile also records
# a vault, a second deployment is bound to it, with its own API key.
set -eu

admin() { fermah-pay-stellar-admin --database-url "$ISSUER_DATABASE_URL" "$@"; }

provision_vault() {
    vault=/profile/vault-deployment.json
    if [ ! -f "$vault" ]; then
        return 0
    fi
    vault_contract=$(jq -r .contract "$vault")
    # Bound already, unless the profile now records another vault.
    if [ -f /state/vault.json ] && [ "$(jq -r .contract /state/vault.json)" = "$vault_contract" ]; then
        return 0
    fi
    # A product of its own per vault, named after it: a stack provisioned
    # before the vault existed kept no product identifier, and product names
    # are unique.
    vault_product=$(admin create-product --name "dev-vault-$(printf %.8s "$vault_contract" | tr A-Z a-z)" \
        | jq -r .product_id)
    vault_id=$(admin create-deployment --product-id "$vault_product" \
        --name testnet-vault --network stellar:testnet | jq -r .deployment_id)
    admin bind-ledger --deployment-id "$vault_id" --contract "$vault_contract" --vault \
        --operator "$(jq -r .operator "$vault")" > /dev/null
    vault_key=$(admin issue-api-key --deployment-id "$vault_id" --label dev-vault)
    umask 077
    jq -n --arg deployment_id "$vault_id" --arg contract "$vault_contract" \
        --argjson key "$vault_key" \
        '{deployment_id: $deployment_id, contract: $contract, api_key: $key.token}' \
        > /state/vault.json
    echo "provisioned vault deployment $vault_id"
}

if [ -f /state/seller.json ]; then
    echo "already provisioned:"
    jq '{deployment_id, contract}' /state/seller.json
    provision_vault
    exit 0
fi

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
provision_vault
