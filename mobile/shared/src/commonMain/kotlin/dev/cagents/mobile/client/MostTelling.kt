package dev.cagents.mobile.client

/**
 * Of the failures at each of a computer's addresses, the one to report.
 *
 * A host key that doesn't match comes first, since it may mean an
 * impersonator; then one to trust; then a refusal; and only then that
 * nothing answered.
 *
 * A [paired] computer's addresses came from its pairing code, local network
 * addresses among them. Away from that network, another machine may answer
 * at one of them while the computer itself is out of reach. So for a paired
 * computer a mismatch counts as an impersonation only when every address
 * presented a wrong key, or a Tailscale address did, whose identity the
 * tailnet vouches for. Otherwise the computer just wasn't reached, and the
 * connection keeps retrying, as it would with nothing answering.
 */
fun mostTelling(failures: List<Throwable>, name: String, hosts: List<String>, paired: Boolean): Throwable {
    val changed = failures.filterIsInstance<ConnectFailure.HostKeyChanged>()
    if (changed.isNotEmpty()) {
        if (!paired || changed.size == failures.size || changed.any { isTailscale(it.host) }) return changed.first()
        val strangers = changed.map { it.host }.filter { it.isNotEmpty() }.distinct()
        val where = if (strangers.isEmpty()) "one of its addresses" else strangers.joinToString(", ")
        return ConnectFailure.Network(
            "Couldn't reach $name. A different computer answered at $where, so this phone may be on another network. " +
                "Is this phone on Tailscale?",
            changed.first(),
        )
    }
    return failures.firstOrNull { it is ConnectFailure.UnknownHostKey }
        ?: failures.firstOrNull { it is ConnectFailure && it !is ConnectFailure.Network }
        ?: failures.singleOrNull()
        ?: ConnectFailure.Network(
            "Couldn't reach $name at any of its ${hosts.size} addresses. " +
                "Is it awake, and is this phone on its network or on Tailscale?",
            failures.firstOrNull(),
        )
}

/** A Tailscale MagicDNS name, or an address in Tailscale's IPv4 (100.64.0.0/10) or IPv6 (fd7a:115c:a1e0::/48) range. */
fun isTailscale(host: String): Boolean {
    val h = host.trim().lowercase().removePrefix("[").removeSuffix("]")
    if (h.endsWith(".ts.net")) return true
    if (h.startsWith("fd7a:115c:a1e0:")) return true
    val octets = h.split('.').takeIf { it.size == 4 }?.map { it.toIntOrNull() ?: return false } ?: return false
    return octets[0] == 100 && octets[1] in 64..127
}
