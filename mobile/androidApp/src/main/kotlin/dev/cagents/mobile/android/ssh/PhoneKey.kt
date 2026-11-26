package dev.cagents.mobile.android.ssh

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.io.File
import java.security.InvalidKeyException
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.SecureRandom
import java.security.Signature
import java.security.UnrecoverableKeyException
import java.security.spec.ECGenParameterSpec
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The phone's own Ed25519 SSH key, made on first use and never exported.
 *
 * Where the Android Keystore can hold an Ed25519 key (Android 13 and later
 * with KeyMint 2), the private key lives there and signing happens in the
 * secure hardware. Otherwise the key is generated in software and its seed is
 * stored encrypted with an AES key that the Keystore holds.
 *
 * A key once made is never replaced silently, since computers trust it: if
 * it stops working, loading it fails with [PhoneKeyUnusable] until the
 * person asks for a new one with [replace].
 */
class PhoneKey private constructor(
    override val publicKey: ByteArray,
    private val signer: (ByteArray) -> ByteArray,
    /** True when the private key stays inside the Android Keystore. */
    val hardwareBacked: Boolean,
) : SigningKey {
    override fun sign(data: ByteArray): ByteArray = signer(data)

    /** The key as one `authorized_keys` entry, without options. */
    val openSsh: String
        get() = "$openSshKey $COMMENT"

    companion object {
        const val COMMENT = "cagents-phone"
        private const val KEYSTORE = "AndroidKeyStore"
        private const val SIGNING_ALIAS = "cagents-ssh-ed25519"
        private const val WRAPPING_ALIAS = "cagents-ssh-seed-wrap"
        private const val SEED_FILE = "ssh-ed25519.seed"

        @Volatile
        private var loaded: PhoneKey? = null

        /**
         * Loads the phone's key, making it on first use. Blocking: call off
         * the main thread. Throws [PhoneKeyUnusable] if the key it has can't
         * be used.
         */
        fun get(context: Context): PhoneKey = loaded ?: synchronized(this) {
            loaded ?: load(context.applicationContext).also { loaded = it }
        }

        /**
         * Discards the phone's key, whatever state it is in, and makes a new
         * one. Computers that trusted the old key won't accept the new one
         * until it is added there. Blocking: call off the main thread.
         */
        fun replace(context: Context): PhoneKey = synchronized(this) {
            loaded = null
            val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
            for (alias in listOf(SIGNING_ALIAS, WRAPPING_ALIAS)) runCatching { keyStore.deleteEntry(alias) }
            val seedFile = File(context.applicationContext.noBackupFilesDir, SEED_FILE)
            File(seedFile.parentFile, "${seedFile.name}.tmp").delete()
            check(!seedFile.exists() || seedFile.delete()) { "Couldn't remove the old key" }
            load(context.applicationContext).also { loaded = it }
        }

        private fun load(context: Context): PhoneKey {
            val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
            val seedFile = File(context.noBackupFilesDir, SEED_FILE)
            if (!seedFile.exists()) {
                keystoreKey(keyStore)?.let { return it }
            }
            return softwareKey(keyStore, seedFile)
        }

        private fun keystoreKey(keyStore: KeyStore): PhoneKey? {
            if (android.os.Build.VERSION.SDK_INT < 33) return null
            val existed = keyStore.containsAlias(SIGNING_ALIAS)
            return runCatching {
                if (!existed) {
                    KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, KEYSTORE).apply {
                        initialize(
                            KeyGenParameterSpec.Builder(SIGNING_ALIAS, KeyProperties.PURPOSE_SIGN)
                                .setAlgorithmParameterSpec(ECGenParameterSpec("ed25519"))
                                .setDigests(KeyProperties.DIGEST_NONE)
                                .build(),
                        )
                    }.generateKeyPair()
                }
                val private = keyStore.getKey(SIGNING_ALIAS, null) as PrivateKey
                val encoded = keyStore.getCertificate(SIGNING_ALIAS).publicKey.encoded
                val raw = encoded.copyOfRange(encoded.size - 32, encoded.size)
                val key = PhoneKey(raw, { data -> keystoreSign(private, data) }, hardwareBacked = true)
                // Some devices create the key but can't sign with it; find out now.
                key.sign(byteArrayOf(0))
                key
            }.onFailure {
                // Only a key made just now is discarded: an older one is already authorized somewhere.
                if (!existed) runCatching { keyStore.deleteEntry(SIGNING_ALIAS) }
            }.getOrElse { if (existed) throw PhoneKeyUnusable(it) else null }
        }

        private fun keystoreSign(key: PrivateKey, data: ByteArray): ByteArray =
            Signature.getInstance("Ed25519").run {
                initSign(key)
                update(data)
                sign()
            }

        private fun softwareKey(keyStore: KeyStore, seedFile: File): PhoneKey {
            val seed = if (seedFile.exists()) {
                // A missing wrapping key isn't made afresh here: it couldn't open this seed.
                val wrapping = try {
                    keyStore.getKey(WRAPPING_ALIAS, null) as SecretKey?
                } catch (e: UnrecoverableKeyException) {
                    throw PhoneKeyUnusable(e)
                }
                unsealSeed(seedFile.readBytes(), wrapping)
            } else {
                val wrapping = wrappingKey(keyStore)
                val fresh = ByteArray(32).also { SecureRandom().nextBytes(it) }
                val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, wrapping) }
                val sealed = cipher.iv + cipher.doFinal(fresh)
                val temporary = File(seedFile.parentFile, "${seedFile.name}.tmp")
                temporary.writeBytes(sealed)
                check(temporary.renameTo(seedFile)) { "Couldn't store the phone's key" }
                fresh
            }
            val private = Ed25519PrivateKeyParameters(seed, 0)
            val public = private.generatePublicKey().encoded
            return PhoneKey(public, { data ->
                Ed25519Signer().run {
                    init(true, private)
                    update(data, 0, data.size)
                    generateSignature()
                }
            }, hardwareBacked = false)
        }

        /**
         * The seed in [stored], as [softwareKey] seals it: the GCM IV, then the
         * encrypted seed and its tag. Throws [PhoneKeyUnusable] when it can't
         * be opened with [wrapping], or [wrapping] is gone; other failures,
         * which may pass, are thrown as they are.
         */
        internal fun unsealSeed(stored: ByteArray, wrapping: SecretKey?): ByteArray {
            if (wrapping == null) throw PhoneKeyUnusable(IllegalStateException("The key that sealed it is gone."))
            val seed = try {
                Cipher.getInstance("AES/GCM/NoPadding").run {
                    init(Cipher.DECRYPT_MODE, wrapping, GCMParameterSpec(128, stored, 0, 12))
                    doFinal(stored, 12, stored.size - 12)
                }
            } catch (e: AEADBadTagException) {
                // Not sealed with this key, or changed since.
                throw PhoneKeyUnusable(e)
            } catch (e: InvalidKeyException) {
                // Such as a Keystore key permanently invalidated.
                throw PhoneKeyUnusable(e)
            } catch (e: IllegalArgumentException) {
                // Too short to hold an IV.
                throw PhoneKeyUnusable(e)
            }
            if (seed.size != 32) throw PhoneKeyUnusable(IllegalStateException("The stored seed is ${seed.size} bytes."))
            return seed
        }

        private fun wrappingKey(keyStore: KeyStore): SecretKey {
            (keyStore.getKey(WRAPPING_ALIAS, null) as? SecretKey)?.let { return it }
            return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE).apply {
                init(
                    KeyGenParameterSpec.Builder(
                        WRAPPING_ALIAS,
                        KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                    )
                        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                        .setKeySize(256)
                        .build(),
                )
            }.generateKey()
        }

        /** The SSH wire encoding of an Ed25519 public key: its type and its 32 bytes. */
        fun publicBlob(raw: ByteArray): ByteArray = ByteArrayOutputStream().also { bytes ->
            DataOutputStream(bytes).run {
                val type = "ssh-ed25519".toByteArray()
                writeInt(type.size)
                write(type)
                writeInt(raw.size)
                write(raw)
            }
        }.toByteArray()
    }
}

/** The phone's key exists but can't be used, such as after the Keystore lost what it held. */
class PhoneKeyUnusable(cause: Throwable) :
    Exception("This phone's key can't be used anymore. Make a new one in Settings.", cause)
