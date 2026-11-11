//! The committed update key must be a real minisign public key. A placeholder would make every
//! installed copy refuse every update (it fails closed), so a release must never carry one.

#[test]
fn the_committed_update_key_is_a_real_public_key() {
    let key = navigator_update::PUBKEY;
    assert!(
        minisign_verify::PublicKey::decode(key).is_ok(),
        "assets/update/minisign.pub is not a minisign public key; the release key is missing"
    );
}
