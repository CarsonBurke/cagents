package dev.cagents.mobile.client

/**
 * Where OpenSSH keeps a computer's public host key of [keyType], such as
 * `ssh-ed25519`, for `ssh-keygen -lf` to print its fingerprint. A host
 * certificate's type names the file of the key it certifies. Null for a
 * type OpenSSH has no host key file for.
 */
fun hostKeyFile(keyType: String): String? {
    val name = when (keyType.removeSuffix(CERTIFICATE_SUFFIX)) {
        "ssh-ed25519" -> "ed25519"
        "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521" -> "ecdsa"
        "ssh-rsa", "rsa-sha2-256", "rsa-sha2-512" -> "rsa"
        "ssh-dss" -> "dsa"
        else -> return null
    }
    return "/etc/ssh/ssh_host_${name}_key.pub"
}

private const val CERTIFICATE_SUFFIX = "-cert-v01@openssh.com"
