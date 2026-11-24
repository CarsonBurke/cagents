package dev.cagents.mobile.android.ssh

import android.content.Context
import android.util.Base64
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import net.schmizz.sshj.common.Buffer
import java.security.MessageDigest
import java.security.PublicKey

/**
 * The host keys the phone trusts, one per computer address, shown as
 * OpenSSH's SHA-256 fingerprint so it can be compared with
 * `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the computer. A
 * pairing code pins its computer's key; a computer set up by hand is
 * trusted on first use.
 */
class HostKeyStore(context: Context) {
    private val prefs = context.getSharedPreferences("host_keys", Context.MODE_PRIVATE)
    private val _revision = MutableStateFlow(0)

    /** Changes whenever a pin does. */
    val revision: StateFlow<Int> = _revision.asStateFlow()

    fun pinned(hostKeyId: String): String? = prefs.getString("$hostKeyId/fingerprint", null)

    fun pinnedType(hostKeyId: String): String? = prefs.getString("$hostKeyId/type", null)

    fun pin(hostKeyIds: Collection<String>, fingerprint: String, keyType: String) {
        val edit = prefs.edit()
        for (id in hostKeyIds) edit.putString("$id/fingerprint", fingerprint).putString("$id/type", keyType)
        edit.apply()
        _revision.value++
    }

    fun forget(hostKeyIds: Collection<String>) {
        val edit = prefs.edit()
        for (id in hostKeyIds) edit.remove("$id/fingerprint").remove("$id/type")
        edit.apply()
        _revision.value++
    }

    companion object {
        const val ED25519 = "ssh-ed25519"

        fun fingerprint(key: PublicKey): String = fingerprint(Buffer.PlainBuffer().putPublicKey(key).compactData)

        /** OpenSSH's form: `SHA256:` and the unpadded base64 of the key blob's digest. */
        fun fingerprint(blob: ByteArray): String {
            val digest = MessageDigest.getInstance("SHA-256").digest(blob)
            return "SHA256:" + Base64.encodeToString(digest, Base64.NO_WRAP or Base64.NO_PADDING)
        }
    }
}
