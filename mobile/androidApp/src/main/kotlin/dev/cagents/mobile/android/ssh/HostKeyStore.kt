package dev.cagents.mobile.android.ssh

import android.content.Context
import android.util.Base64
import net.schmizz.sshj.common.Buffer
import java.security.MessageDigest
import java.security.PublicKey

/**
 * The host keys the person has trusted, one per computer address: trust on
 * first use, shown as OpenSSH's SHA-256 fingerprint so it can be compared
 * with `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the computer.
 */
class HostKeyStore(context: Context) {
    private val prefs = context.getSharedPreferences("host_keys", Context.MODE_PRIVATE)

    fun pinned(hostKeyId: String): String? = prefs.getString("$hostKeyId/fingerprint", null)

    fun pinnedType(hostKeyId: String): String? = prefs.getString("$hostKeyId/type", null)

    fun pin(hostKeyId: String, fingerprint: String, keyType: String) {
        prefs.edit()
            .putString("$hostKeyId/fingerprint", fingerprint)
            .putString("$hostKeyId/type", keyType)
            .apply()
    }

    fun forget(hostKeyId: String) {
        prefs.edit().remove("$hostKeyId/fingerprint").remove("$hostKeyId/type").apply()
    }

    companion object {
        /** OpenSSH's form: `SHA256:` and the unpadded base64 of the key blob's digest. */
        fun fingerprint(key: PublicKey): String {
            val blob = Buffer.PlainBuffer().putPublicKey(key).compactData
            val digest = MessageDigest.getInstance("SHA-256").digest(blob)
            return "SHA256:" + Base64.encodeToString(digest, Base64.NO_WRAP or Base64.NO_PADDING)
        }
    }
}
