"""This module provides some more Pythonic support for SSL.

Object types:

  SSLSocket -- subtype of socket.socket which does SSL over the socket

Exceptions:

  SSLError -- exception raised for I/O errors

Functions:

  cert_time_to_seconds -- convert time string used for certificate
                          notBefore and notAfter functions to integer
                          seconds past the Epoch (the time values
                          returned from time.time())

  get_server_certificate (addr, ssl_version, ca_certs, timeout) -- Retrieve the
                          certificate from the server at the specified
                          address and return it as a PEM-encoded string


Integer constants:

SSL_ERROR_ZERO_RETURN
SSL_ERROR_WANT_READ
SSL_ERROR_WANT_WRITE
SSL_ERROR_WANT_X509_LOOKUP
SSL_ERROR_SYSCALL
SSL_ERROR_SSL
SSL_ERROR_WANT_CONNECT

SSL_ERROR_EOF
SSL_ERROR_INVALID_ERROR_CODE

The following group define certificate requirements that one side is
allowing/requiring from the other side:

CERT_NONE - no certificates from the other side are required (or will
            be looked at if provided)
CERT_OPTIONAL - certificates are not required, but if provided will be
                validated, and if validation fails, the connection will
                also fail
CERT_REQUIRED - certificates are required, and will be validated, and
                if validation fails, the connection will also fail

The following constants identify various SSL protocol variants:

PROTOCOL_SSLv2
PROTOCOL_SSLv3
PROTOCOL_SSLv23
PROTOCOL_TLS
PROTOCOL_TLS_CLIENT
PROTOCOL_TLS_SERVER
PROTOCOL_TLSv1
PROTOCOL_TLSv1_1
PROTOCOL_TLSv1_2

The following constants identify various SSL alert message descriptions as per
http://www.iana.org/assignments/tls-parameters/tls-parameters.xml#tls-parameters-6

ALERT_DESCRIPTION_CLOSE_NOTIFY
ALERT_DESCRIPTION_UNEXPECTED_MESSAGE
ALERT_DESCRIPTION_BAD_RECORD_MAC
ALERT_DESCRIPTION_RECORD_OVERFLOW
ALERT_DESCRIPTION_DECOMPRESSION_FAILURE
ALERT_DESCRIPTION_HANDSHAKE_FAILURE
ALERT_DESCRIPTION_BAD_CERTIFICATE
ALERT_DESCRIPTION_UNSUPPORTED_CERTIFICATE
ALERT_DESCRIPTION_CERTIFICATE_REVOKED
ALERT_DESCRIPTION_CERTIFICATE_EXPIRED
ALERT_DESCRIPTION_CERTIFICATE_UNKNOWN
ALERT_DESCRIPTION_ILLEGAL_PARAMETER
ALERT_DESCRIPTION_UNKNOWN_CA
ALERT_DESCRIPTION_ACCESS_DENIED
ALERT_DESCRIPTION_DECODE_ERROR
ALERT_DESCRIPTION_DECRYPT_ERROR
ALERT_DESCRIPTION_PROTOCOL_VERSION
ALERT_DESCRIPTION_INSUFFICIENT_SECURITY
ALERT_DESCRIPTION_INTERNAL_ERROR
ALERT_DESCRIPTION_USER_CANCELLED
ALERT_DESCRIPTION_NO_RENEGOTIATION
ALERT_DESCRIPTION_UNSUPPORTED_EXTENSION
ALERT_DESCRIPTION_CERTIFICATE_UNOBTAINABLE
ALERT_DESCRIPTION_UNRECOGNIZED_NAME
ALERT_DESCRIPTION_BAD_CERTIFICATE_STATUS_RESPONSE
ALERT_DESCRIPTION_BAD_CERTIFICATE_HASH_VALUE
ALERT_DESCRIPTION_UNKNOWN_PSK_IDENTITY
"""

import enum
import socket
import sys
from _ssl import (
    _DEFAULT_CIPHERS as _DEFAULT_CIPHERS,
    _OPENSSL_API_VERSION as _OPENSSL_API_VERSION,
    HAS_ALPN as HAS_ALPN,
    HAS_ECDH as HAS_ECDH,
    HAS_NPN as HAS_NPN,
    HAS_SNI as HAS_SNI,
    OPENSSL_VERSION as OPENSSL_VERSION,
    OPENSSL_VERSION_INFO as OPENSSL_VERSION_INFO,
    OPENSSL_VERSION_NUMBER as OPENSSL_VERSION_NUMBER,
    HAS_SSLv2 as HAS_SSLv2,
    HAS_SSLv3 as HAS_SSLv3,
    HAS_TLSv1 as HAS_TLSv1,
    HAS_TLSv1_1 as HAS_TLSv1_1,
    HAS_TLSv1_2 as HAS_TLSv1_2,
    HAS_TLSv1_3 as HAS_TLSv1_3,
    MemoryBIO as MemoryBIO,
    RAND_add as RAND_add,
    RAND_bytes as RAND_bytes,
    RAND_status as RAND_status,
    SSLSession as SSLSession,
    _PasswordType as _PasswordType,  # typeshed only, but re-export for other type stubs to use
    _SSLContext,
)
from _typeshed import ReadableBuffer, StrOrBytesPath, WriteableBuffer
from collections.abc import Callable, Iterable
from typing import Any, Literal, NamedTuple, TypedDict, overload, type_check_only
from typing_extensions import Never, Self, TypeAlias, deprecated

if sys.version_info >= (3, 13):
    from _ssl import HAS_PSK as HAS_PSK

if sys.version_info < (3, 12):
    from _ssl import RAND_pseudo_bytes as RAND_pseudo_bytes

if sys.version_info < (3, 10):
    from _ssl import RAND_egd as RAND_egd

if sys.platform == "win32":
    from _ssl import enum_certificates as enum_certificates, enum_crls as enum_crls

_PCTRTT: TypeAlias = tuple[tuple[str, str], ...]
_PCTRTTT: TypeAlias = tuple[_PCTRTT, ...]
_PeerCertRetDictType: TypeAlias = dict[str, str | _PCTRTTT | _PCTRTT]
_PeerCertRetType: TypeAlias = _PeerCertRetDictType | bytes | None
_SrvnmeCbType: TypeAlias = Callable[[SSLSocket | SSLObject, str | None, SSLSocket], int | None]

socket_error = OSError

class _Cipher(TypedDict):
    aead: bool
    alg_bits: int
    auth: str
    description: str
    digest: str | None
    id: int
    kea: str
    name: str
    protocol: str
    strength_bits: int
    symmetric: str

class SSLError(OSError):
    """An error occurred in the SSL implementation."""

    library: str
    reason: str

class SSLZeroReturnError(SSLError):
    """SSL/TLS session closed cleanly."""

class SSLWantReadError(SSLError):
    """Non-blocking SSL socket needs to read more data
    before the requested operation can be completed.
    """

class SSLWantWriteError(SSLError):
    """Non-blocking SSL socket needs to write more data
    before the requested operation can be completed.
    """

class SSLSyscallError(SSLError):
    """System error when attempting SSL operation."""

class SSLEOFError(SSLError):
    """SSL/TLS connection terminated abruptly."""

class SSLCertVerificationError(SSLError, ValueError):
    """A certificate could not be verified."""

    verify_code: int
    verify_message: str

CertificateError = SSLCertVerificationError

if sys.version_info < (3, 12):
    @deprecated("Deprecated since Python 3.7. Removed in Python 3.12. Use `SSLContext.wrap_socket()` instead.")
    def wrap_socket(
        sock: socket.socket,
        keyfile: StrOrBytesPath | None = None,
        certfile: StrOrBytesPath | None = None,
        server_side: bool = False,
        cert_reqs: int = ...,
        ssl_version: int = ...,
        ca_certs: str | None = None,
        do_handshake_on_connect: bool = True,
        suppress_ragged_eofs: bool = True,
        ciphers: str | None = None,
    ) -> SSLSocket: ...
    @deprecated("Deprecated since Python 3.7. Removed in Python 3.12.")
    def match_hostname(cert: _PeerCertRetDictType, hostname: str) -> None:
        """Verify that *cert* (in decoded format as returned by
        SSLSocket.getpeercert()) matches the *hostname*.  RFC 2818 and RFC 6125
        rules are followed.

        The function matches IP addresses rather than dNSNames if hostname is a
        valid ipaddress string. IPv4 addresses are supported on all platforms.
        IPv6 addresses are supported on platforms with IPv6 support (AF_INET6
        and inet_pton).

        CertificateError is raised on failure. On success, the function
        returns nothing.
        """

def cert_time_to_seconds(cert_time: str) -> int:
    """Return the time in seconds since the Epoch, given the timestring
    representing the "notBefore" or "notAfter" date from a certificate
    in ``"%b %d %H:%M:%S %Y %Z"`` strptime format (C locale).

    "notBefore" or "notAfter" dates must use UTC (RFC 5280).

    Month is one of: Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec
    UTC should be specified as GMT (see ASN1_TIME_print())
    """

if sys.version_info >= (3, 10):
    def get_server_certificate(
        addr: tuple[str, int], ssl_version: int = ..., ca_certs: str | None = None, timeout: float = ...
    ) -> str:
        """Retrieve the certificate from the server at the specified address,
        and return it as a PEM-encoded string.
        If 'ca_certs' is specified, validate the server cert against it.
        If 'ssl_version' is specified, use it in the connection attempt.
        If 'timeout' is specified, use it in the connection attempt.
        """

else:
    def get_server_certificate(addr: tuple[str, int], ssl_version: int = ..., ca_certs: str | None = None) -> str:
        """Retrieve the certificate from the server at the specified address,
        and return it as a PEM-encoded string.
        If 'ca_certs' is specified, validate the server cert against it.
        If 'ssl_version' is specified, use it in the connection attempt.
        """

def DER_cert_to_PEM_cert(der_cert_bytes: ReadableBuffer) -> str:
    """Takes a certificate in binary DER format and returns the
    PEM version of it as a string.
    """

def PEM_cert_to_DER_cert(pem_cert_string: str) -> bytes:
    """Takes a certificate in ASCII PEM format and returns the
    DER-encoded version of it as a byte sequence
    """

class DefaultVerifyPaths(NamedTuple):
    """DefaultVerifyPaths(cafile, capath, openssl_cafile_env, openssl_cafile, openssl_capath_env, openssl_capath)"""

    cafile: str
    capath: str
    openssl_cafile_env: str
    openssl_cafile: str
    openssl_capath_env: str
    openssl_capath: str

def get_default_verify_paths() -> DefaultVerifyPaths:
    """Return paths to default cafile and capath."""

class VerifyMode(enum.IntEnum):
    """An enumeration."""

    CERT_NONE = 0
    CERT_OPTIONAL = 1
    CERT_REQUIRED = 2

CERT_NONE: VerifyMode
CERT_OPTIONAL: VerifyMode
CERT_REQUIRED: VerifyMode

class VerifyFlags(enum.IntFlag):
    """An enumeration."""

    VERIFY_DEFAULT = 0
    VERIFY_CRL_CHECK_LEAF = 4
    VERIFY_CRL_CHECK_CHAIN = 12
    VERIFY_X509_STRICT = 32
    VERIFY_X509_TRUSTED_FIRST = 32768
    if sys.version_info >= (3, 10):
        VERIFY_ALLOW_PROXY_CERTS = 64
        VERIFY_X509_PARTIAL_CHAIN = 524288

VERIFY_DEFAULT: VerifyFlags
VERIFY_CRL_CHECK_LEAF: VerifyFlags
VERIFY_CRL_CHECK_CHAIN: VerifyFlags
VERIFY_X509_STRICT: VerifyFlags
VERIFY_X509_TRUSTED_FIRST: VerifyFlags

if sys.version_info >= (3, 10):
    VERIFY_ALLOW_PROXY_CERTS: VerifyFlags
    VERIFY_X509_PARTIAL_CHAIN: VerifyFlags

class _SSLMethod(enum.IntEnum):
    """An enumeration."""

    PROTOCOL_SSLv23 = 2
    PROTOCOL_SSLv2 = ...
    PROTOCOL_SSLv3 = ...
    PROTOCOL_TLSv1 = 3
    PROTOCOL_TLSv1_1 = 4
    PROTOCOL_TLSv1_2 = 5
    PROTOCOL_TLS = 2
    PROTOCOL_TLS_CLIENT = 16
    PROTOCOL_TLS_SERVER = 17

PROTOCOL_SSLv23: _SSLMethod
PROTOCOL_SSLv2: _SSLMethod
PROTOCOL_SSLv3: _SSLMethod
PROTOCOL_TLSv1: _SSLMethod
PROTOCOL_TLSv1_1: _SSLMethod
PROTOCOL_TLSv1_2: _SSLMethod
PROTOCOL_TLS: _SSLMethod
PROTOCOL_TLS_CLIENT: _SSLMethod
PROTOCOL_TLS_SERVER: _SSLMethod

class Options(enum.IntFlag):
    """An enumeration."""

    OP_ALL = 2147483728
    OP_NO_SSLv2 = 0
    OP_NO_SSLv3 = 33554432
    OP_NO_TLSv1 = 67108864
    OP_NO_TLSv1_1 = 268435456
    OP_NO_TLSv1_2 = 134217728
    OP_NO_TLSv1_3 = 536870912
    OP_CIPHER_SERVER_PREFERENCE = 4194304
    OP_SINGLE_DH_USE = 0
    OP_SINGLE_ECDH_USE = 0
    OP_NO_COMPRESSION = 131072
    OP_NO_TICKET = 16384
    OP_NO_RENEGOTIATION = 1073741824
    OP_ENABLE_MIDDLEBOX_COMPAT = 1048576
    if sys.version_info >= (3, 12):
        OP_LEGACY_SERVER_CONNECT = 4
        OP_ENABLE_KTLS = 8
    if sys.version_info >= (3, 11) or sys.platform == "linux":
        OP_IGNORE_UNEXPECTED_EOF = 128

OP_ALL: Options
OP_NO_SSLv2: Options
OP_NO_SSLv3: Options
OP_NO_TLSv1: Options
OP_NO_TLSv1_1: Options
OP_NO_TLSv1_2: Options
OP_NO_TLSv1_3: Options
OP_CIPHER_SERVER_PREFERENCE: Options
OP_SINGLE_DH_USE: Options
OP_SINGLE_ECDH_USE: Options
OP_NO_COMPRESSION: Options
OP_NO_TICKET: Options
OP_NO_RENEGOTIATION: Options
OP_ENABLE_MIDDLEBOX_COMPAT: Options
if sys.version_info >= (3, 12):
    OP_LEGACY_SERVER_CONNECT: Options
    OP_ENABLE_KTLS: Options
if sys.version_info >= (3, 11) or sys.platform == "linux":
    OP_IGNORE_UNEXPECTED_EOF: Options

HAS_NEVER_CHECK_COMMON_NAME: bool

CHANNEL_BINDING_TYPES: list[str]

class AlertDescription(enum.IntEnum):
    """An enumeration."""

    ALERT_DESCRIPTION_ACCESS_DENIED = 49
    ALERT_DESCRIPTION_BAD_CERTIFICATE = 42
    ALERT_DESCRIPTION_BAD_CERTIFICATE_HASH_VALUE = 114
    ALERT_DESCRIPTION_BAD_CERTIFICATE_STATUS_RESPONSE = 113
    ALERT_DESCRIPTION_BAD_RECORD_MAC = 20
    ALERT_DESCRIPTION_CERTIFICATE_EXPIRED = 45
    ALERT_DESCRIPTION_CERTIFICATE_REVOKED = 44
    ALERT_DESCRIPTION_CERTIFICATE_UNKNOWN = 46
    ALERT_DESCRIPTION_CERTIFICATE_UNOBTAINABLE = 111
    ALERT_DESCRIPTION_CLOSE_NOTIFY = 0
    ALERT_DESCRIPTION_DECODE_ERROR = 50
    ALERT_DESCRIPTION_DECOMPRESSION_FAILURE = 30
    ALERT_DESCRIPTION_DECRYPT_ERROR = 51
    ALERT_DESCRIPTION_HANDSHAKE_FAILURE = 40
    ALERT_DESCRIPTION_ILLEGAL_PARAMETER = 47
    ALERT_DESCRIPTION_INSUFFICIENT_SECURITY = 71
    ALERT_DESCRIPTION_INTERNAL_ERROR = 80
    ALERT_DESCRIPTION_NO_RENEGOTIATION = 100
    ALERT_DESCRIPTION_PROTOCOL_VERSION = 70
    ALERT_DESCRIPTION_RECORD_OVERFLOW = 22
    ALERT_DESCRIPTION_UNEXPECTED_MESSAGE = 10
    ALERT_DESCRIPTION_UNKNOWN_CA = 48
    ALERT_DESCRIPTION_UNKNOWN_PSK_IDENTITY = 115
    ALERT_DESCRIPTION_UNRECOGNIZED_NAME = 112
    ALERT_DESCRIPTION_UNSUPPORTED_CERTIFICATE = 43
    ALERT_DESCRIPTION_UNSUPPORTED_EXTENSION = 110
    ALERT_DESCRIPTION_USER_CANCELLED = 90

ALERT_DESCRIPTION_HANDSHAKE_FAILURE: AlertDescription
ALERT_DESCRIPTION_INTERNAL_ERROR: AlertDescription
ALERT_DESCRIPTION_ACCESS_DENIED: AlertDescription
ALERT_DESCRIPTION_BAD_CERTIFICATE: AlertDescription
ALERT_DESCRIPTION_BAD_CERTIFICATE_HASH_VALUE: AlertDescription
ALERT_DESCRIPTION_BAD_CERTIFICATE_STATUS_RESPONSE: AlertDescription
ALERT_DESCRIPTION_BAD_RECORD_MAC: AlertDescription
ALERT_DESCRIPTION_CERTIFICATE_EXPIRED: AlertDescription
ALERT_DESCRIPTION_CERTIFICATE_REVOKED: AlertDescription
ALERT_DESCRIPTION_CERTIFICATE_UNKNOWN: AlertDescription
ALERT_DESCRIPTION_CERTIFICATE_UNOBTAINABLE: AlertDescription
ALERT_DESCRIPTION_CLOSE_NOTIFY: AlertDescription
ALERT_DESCRIPTION_DECODE_ERROR: AlertDescription
ALERT_DESCRIPTION_DECOMPRESSION_FAILURE: AlertDescription
ALERT_DESCRIPTION_DECRYPT_ERROR: AlertDescription
ALERT_DESCRIPTION_ILLEGAL_PARAMETER: AlertDescription
ALERT_DESCRIPTION_INSUFFICIENT_SECURITY: AlertDescription
ALERT_DESCRIPTION_NO_RENEGOTIATION: AlertDescription
ALERT_DESCRIPTION_PROTOCOL_VERSION: AlertDescription
ALERT_DESCRIPTION_RECORD_OVERFLOW: AlertDescription
ALERT_DESCRIPTION_UNEXPECTED_MESSAGE: AlertDescription
ALERT_DESCRIPTION_UNKNOWN_CA: AlertDescription
ALERT_DESCRIPTION_UNKNOWN_PSK_IDENTITY: AlertDescription
ALERT_DESCRIPTION_UNRECOGNIZED_NAME: AlertDescription
ALERT_DESCRIPTION_UNSUPPORTED_CERTIFICATE: AlertDescription
ALERT_DESCRIPTION_UNSUPPORTED_EXTENSION: AlertDescription
ALERT_DESCRIPTION_USER_CANCELLED: AlertDescription

# This class is not exposed. It calls itself ssl._ASN1Object.
@type_check_only
class _ASN1ObjectBase(NamedTuple):
    nid: int
    shortname: str
    longname: str
    oid: str

class _ASN1Object(_ASN1ObjectBase):
    """ASN.1 object identifier lookup"""

    def __new__(cls, oid: str) -> Self: ...
    @classmethod
    def fromnid(cls, nid: int) -> Self:
        """Create _ASN1Object from OpenSSL numeric ID"""

    @classmethod
    def fromname(cls, name: str) -> Self:
        """Create _ASN1Object from short name, long name or OID"""

class Purpose(_ASN1Object, enum.Enum):
    """SSLContext purpose flags with X509v3 Extended Key Usage objects"""

    # Normally this class would inherit __new__ from _ASN1Object, but
    # because this is an enum, the inherited __new__ is replaced at runtime with
    # Enum.__new__.
    def __new__(cls, value: object) -> Self: ...
    SERVER_AUTH = (129, "serverAuth", "TLS Web Server Authentication", "1.3.6.1.5.5.7.3.2")  # pyright: ignore[reportCallIssue]
    CLIENT_AUTH = (130, "clientAuth", "TLS Web Client Authentication", "1.3.6.1.5.5.7.3.1")  # pyright: ignore[reportCallIssue]

class SSLSocket(socket.socket):
    """This class implements a subtype of socket.socket that wraps
    the underlying OS socket in an SSL context when necessary, and
    provides read and write methods over that channel.
    """

    context: SSLContext
    server_side: bool
    server_hostname: str | None
    session: SSLSession | None
    @property
    def session_reused(self) -> bool | None:
        """Was the client session reused during handshake"""

    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
    def connect(self, addr: socket._Address) -> None:
        """Connects to remote ADDR, and then wraps the connection in
        an SSL channel.
        """

    def connect_ex(self, addr: socket._Address) -> int:
        """Connects to remote ADDR, and then wraps the connection in
        an SSL channel.
        """

    def recv(self, buflen: int = 1024, flags: int = 0) -> bytes: ...
    def recv_into(self, buffer: WriteableBuffer, nbytes: int | None = None, flags: int = 0) -> int: ...
    def recvfrom(self, buflen: int = 1024, flags: int = 0) -> tuple[bytes, socket._RetAddress]: ...
    def recvfrom_into(
        self, buffer: WriteableBuffer, nbytes: int | None = None, flags: int = 0
    ) -> tuple[int, socket._RetAddress]: ...
    def send(self, data: ReadableBuffer, flags: int = 0) -> int: ...
    def sendall(self, data: ReadableBuffer, flags: int = 0) -> None: ...
    @overload
    def sendto(self, data: ReadableBuffer, flags_or_addr: socket._Address, addr: None = None) -> int: ...
    @overload
    def sendto(self, data: ReadableBuffer, flags_or_addr: int, addr: socket._Address) -> int: ...
    def shutdown(self, how: int) -> None: ...
    def read(self, len: int = 1024, buffer: bytearray | None = None) -> bytes:
        """Read up to LEN bytes and return them.
        Return zero-length string on EOF.
        """

    def write(self, data: ReadableBuffer) -> int:
        """Write DATA to the underlying SSL channel.  Returns
        number of bytes of DATA actually transmitted.
        """

    def do_handshake(self, block: bool = False) -> None:  # block is undocumented
        """Start the SSL/TLS handshake."""

    @overload
    def getpeercert(self, binary_form: Literal[False] = False) -> _PeerCertRetDictType | None:
        """Returns a formatted version of the data in the certificate provided
        by the other end of the SSL channel.

        Return None if no certificate was provided, {} if a certificate was
        provided, but not validated.
        """

    @overload
    def getpeercert(self, binary_form: Literal[True]) -> bytes | None: ...
    @overload
    def getpeercert(self, binary_form: bool) -> _PeerCertRetType: ...
    def cipher(self) -> tuple[str, str, int] | None:
        """Return the currently selected cipher as a 3-tuple ``(name,
        ssl_version, secret_bits)``.
        """

    def shared_ciphers(self) -> list[tuple[str, str, int]] | None:
        """Return a list of ciphers shared by the client during the handshake or
        None if this is not a valid server connection.
        """

    def compression(self) -> str | None:
        """Return the current compression algorithm in use, or ``None`` if
        compression was not negotiated or not supported by one of the peers.
        """

    def get_channel_binding(self, cb_type: str = "tls-unique") -> bytes | None:
        """Get channel binding data for current connection.  Raise ValueError
        if the requested `cb_type` is not supported.  Return bytes of the data
        or None if the data is not available (e.g. before the handshake).
        """

    def selected_alpn_protocol(self) -> str | None:
        """Return the currently selected ALPN protocol as a string, or ``None``
        if a next protocol was not negotiated or if ALPN is not supported by one
        of the peers.
        """
    if sys.version_info >= (3, 10):
        @deprecated("Deprecated since Python 3.10. Use ALPN instead.")
        def selected_npn_protocol(self) -> str | None:
            """Return the currently selected NPN protocol as a string, or ``None``
            if a next protocol was not negotiated or if NPN is not supported by one
            of the peers.
            """
    else:
        def selected_npn_protocol(self) -> str | None:
            """Return the currently selected NPN protocol as a string, or ``None``
            if a next protocol was not negotiated or if NPN is not supported by one
            of the peers.
            """

    def accept(self) -> tuple[SSLSocket, socket._RetAddress]:
        """Accepts a new connection from a remote client, and returns
        a tuple containing that new connection wrapped with a server-side
        SSL channel, and the address of the remote client.
        """

    def unwrap(self) -> socket.socket:
        """Start the SSL shutdown handshake."""

    def version(self) -> str | None:
        """Return a string identifying the protocol version used by the
        current SSL channel.
        """

    def pending(self) -> int:
        """Return the number of bytes that can be read immediately."""

    def verify_client_post_handshake(self) -> None: ...
    # These methods always raise `NotImplementedError`:
    def recvmsg(self, *args: Never, **kwargs: Never) -> Never: ...  # type: ignore[override]
    def recvmsg_into(self, *args: Never, **kwargs: Never) -> Never: ...  # type: ignore[override]
    def sendmsg(self, *args: Never, **kwargs: Never) -> Never: ...  # type: ignore[override]
    if sys.version_info >= (3, 13):
        def get_verified_chain(self) -> list[bytes]:
            """Returns verified certificate chain provided by the other
            end of the SSL channel as a list of DER-encoded bytes.

            If certificate verification was disabled method acts the same as
            ``SSLSocket.get_unverified_chain``.
            """

        def get_unverified_chain(self) -> list[bytes]:
            """Returns raw certificate chain provided by the other
            end of the SSL channel as a list of DER-encoded bytes.
            """

class TLSVersion(enum.IntEnum):
    """An enumeration."""

    MINIMUM_SUPPORTED = -2
    MAXIMUM_SUPPORTED = -1
    SSLv3 = 768
    TLSv1 = 769
    TLSv1_1 = 770
    TLSv1_2 = 771
    TLSv1_3 = 772

class SSLContext(_SSLContext):
    """An SSLContext holds various SSL-related configuration options and
    data, such as certificates and possibly a private key.
    """

    options: Options
    verify_flags: VerifyFlags
    verify_mode: VerifyMode
    @property
    def protocol(self) -> _SSLMethod: ...  # type: ignore[override]
    hostname_checks_common_name: bool
    maximum_version: TLSVersion
    minimum_version: TLSVersion
    # The following two attributes have class-level defaults.
    # However, the docs explicitly state that it's OK to override these attributes on instances,
    # so making these ClassVars wouldn't be appropriate
    sslobject_class: type[SSLObject]
    sslsocket_class: type[SSLSocket]
    keylog_filename: str
    post_handshake_auth: bool
    if sys.version_info >= (3, 10):
        security_level: int
    if sys.version_info >= (3, 10):
        @overload
        def __new__(cls, protocol: int, *args: Any, **kwargs: Any) -> Self: ...
        @overload
        @deprecated("Deprecated since Python 3.10. Use a specific version of the SSL protocol.")
        def __new__(cls, protocol: None = None, *args: Any, **kwargs: Any) -> Self: ...
    else:
        def __new__(cls, protocol: int = ..., *args: Any, **kwargs: Any) -> Self: ...

    def load_default_certs(self, purpose: Purpose = Purpose.SERVER_AUTH) -> None: ...
    def load_verify_locations(
        self,
        cafile: StrOrBytesPath | None = None,
        capath: StrOrBytesPath | None = None,
        cadata: str | ReadableBuffer | None = None,
    ) -> None: ...
    @overload
    def get_ca_certs(self, binary_form: Literal[False] = False) -> list[_PeerCertRetDictType]:
        """Returns a list of dicts with information of loaded CA certs.

        If the optional argument is True, returns a DER-encoded copy of the CA
        certificate.

        NOTE: Certificates in a capath directory aren't loaded unless they have
        been used at least once.
        """

    @overload
    def get_ca_certs(self, binary_form: Literal[True]) -> list[bytes]: ...
    @overload
    def get_ca_certs(self, binary_form: bool = False) -> Any: ...
    def get_ciphers(self) -> list[_Cipher]: ...
    def set_default_verify_paths(self) -> None: ...
    def set_ciphers(self, cipherlist: str, /) -> None: ...
    def set_alpn_protocols(self, alpn_protocols: Iterable[str]) -> None: ...
    if sys.version_info >= (3, 10):
        @deprecated("Deprecated since Python 3.10. Use ALPN instead.")
        def set_npn_protocols(self, npn_protocols: Iterable[str]) -> None: ...
    else:
        def set_npn_protocols(self, npn_protocols: Iterable[str]) -> None: ...

    def set_servername_callback(self, server_name_callback: _SrvnmeCbType | None) -> None: ...
    def load_dh_params(self, path: str, /) -> None: ...
    def set_ecdh_curve(self, name: str, /) -> None: ...
    def wrap_socket(
        self,
        sock: socket.socket,
        server_side: bool = False,
        do_handshake_on_connect: bool = True,
        suppress_ragged_eofs: bool = True,
        server_hostname: str | bytes | None = None,
        session: SSLSession | None = None,
    ) -> SSLSocket: ...
    def wrap_bio(
        self,
        incoming: MemoryBIO,
        outgoing: MemoryBIO,
        server_side: bool = False,
        server_hostname: str | bytes | None = None,
        session: SSLSession | None = None,
    ) -> SSLObject: ...

def create_default_context(
    purpose: Purpose = Purpose.SERVER_AUTH,
    *,
    cafile: StrOrBytesPath | None = None,
    capath: StrOrBytesPath | None = None,
    cadata: str | ReadableBuffer | None = None,
) -> SSLContext:
    """Create a SSLContext object with default settings.

    NOTE: The protocol and settings may change anytime without prior
          deprecation. The values represent a fair balance between maximum
          compatibility and security.
    """

if sys.version_info >= (3, 10):
    def _create_unverified_context(
        protocol: int | None = None,
        *,
        cert_reqs: int = ...,
        check_hostname: bool = False,
        purpose: Purpose = Purpose.SERVER_AUTH,
        certfile: StrOrBytesPath | None = None,
        keyfile: StrOrBytesPath | None = None,
        cafile: StrOrBytesPath | None = None,
        capath: StrOrBytesPath | None = None,
        cadata: str | ReadableBuffer | None = None,
    ) -> SSLContext:
        """Create a SSLContext object for Python stdlib modules

        All Python stdlib modules shall use this function to create SSLContext
        objects in order to keep common settings in one place. The configuration
        is less restrict than create_default_context()'s to increase backward
        compatibility.
        """

else:
    def _create_unverified_context(
        protocol: int = ...,
        *,
        cert_reqs: int = ...,
        check_hostname: bool = False,
        purpose: Purpose = Purpose.SERVER_AUTH,
        certfile: StrOrBytesPath | None = None,
        keyfile: StrOrBytesPath | None = None,
        cafile: StrOrBytesPath | None = None,
        capath: StrOrBytesPath | None = None,
        cadata: str | ReadableBuffer | None = None,
    ) -> SSLContext:
        """Create a SSLContext object for Python stdlib modules

        All Python stdlib modules shall use this function to create SSLContext
        objects in order to keep common settings in one place. The configuration
        is less restrict than create_default_context()'s to increase backward
        compatibility.
        """

_create_default_https_context = create_default_context

class SSLObject:
    """This class implements an interface on top of a low-level SSL object as
    implemented by OpenSSL. This object captures the state of an SSL connection
    but does not provide any network IO itself. IO needs to be performed
    through separate "BIO" objects which are OpenSSL's IO abstraction layer.

    This class does not have a public constructor. Instances are returned by
    ``SSLContext.wrap_bio``. This class is typically used by framework authors
    that want to implement asynchronous IO for SSL through memory buffers.

    When compared to ``SSLSocket``, this object lacks the following features:

     * Any form of network IO, including methods such as ``recv`` and ``send``.
     * The ``do_handshake_on_connect`` and ``suppress_ragged_eofs`` machinery.
    """

    context: SSLContext
    @property
    def server_side(self) -> bool:
        """Whether this is a server-side socket."""

    @property
    def server_hostname(self) -> str | None:
        """The currently set server hostname (for SNI), or ``None`` if no
        server hostname is set.
        """
    session: SSLSession | None
    @property
    def session_reused(self) -> bool:
        """Was the client session reused during handshake"""

    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
    def read(self, len: int = 1024, buffer: bytearray | None = None) -> bytes:
        """Read up to 'len' bytes from the SSL object and return them.

        If 'buffer' is provided, read into this buffer and return the number of
        bytes read.
        """

    def write(self, data: ReadableBuffer) -> int:
        """Write 'data' to the SSL object and return the number of bytes
        written.

        The 'data' argument must support the buffer interface.
        """

    @overload
    def getpeercert(self, binary_form: Literal[False] = False) -> _PeerCertRetDictType | None:
        """Returns a formatted version of the data in the certificate provided
        by the other end of the SSL channel.

        Return None if no certificate was provided, {} if a certificate was
        provided, but not validated.
        """

    @overload
    def getpeercert(self, binary_form: Literal[True]) -> bytes | None: ...
    @overload
    def getpeercert(self, binary_form: bool) -> _PeerCertRetType: ...
    def selected_alpn_protocol(self) -> str | None:
        """Return the currently selected ALPN protocol as a string, or ``None``
        if a next protocol was not negotiated or if ALPN is not supported by one
        of the peers.
        """
    if sys.version_info >= (3, 10):
        @deprecated("Deprecated since Python 3.10. Use ALPN instead.")
        def selected_npn_protocol(self) -> str | None:
            """Return the currently selected NPN protocol as a string, or ``None``
            if a next protocol was not negotiated or if NPN is not supported by one
            of the peers.
            """
    else:
        def selected_npn_protocol(self) -> str | None:
            """Return the currently selected NPN protocol as a string, or ``None``
            if a next protocol was not negotiated or if NPN is not supported by one
            of the peers.
            """

    def cipher(self) -> tuple[str, str, int] | None:
        """Return the currently selected cipher as a 3-tuple ``(name,
        ssl_version, secret_bits)``.
        """

    def shared_ciphers(self) -> list[tuple[str, str, int]] | None:
        """Return a list of ciphers shared by the client during the handshake or
        None if this is not a valid server connection.
        """

    def compression(self) -> str | None:
        """Return the current compression algorithm in use, or ``None`` if
        compression was not negotiated or not supported by one of the peers.
        """

    def pending(self) -> int:
        """Return the number of bytes that can be read immediately."""

    def do_handshake(self) -> None:
        """Start the SSL/TLS handshake."""

    def unwrap(self) -> None:
        """Start the SSL shutdown handshake."""

    def version(self) -> str | None:
        """Return a string identifying the protocol version used by the
        current SSL channel.
        """

    def get_channel_binding(self, cb_type: str = "tls-unique") -> bytes | None:
        """Get channel binding data for current connection.  Raise ValueError
        if the requested `cb_type` is not supported.  Return bytes of the data
        or None if the data is not available (e.g. before the handshake).
        """

    def verify_client_post_handshake(self) -> None: ...
    if sys.version_info >= (3, 13):
        def get_verified_chain(self) -> list[bytes]:
            """Returns verified certificate chain provided by the other
            end of the SSL channel as a list of DER-encoded bytes.

            If certificate verification was disabled method acts the same as
            ``SSLSocket.get_unverified_chain``.
            """

        def get_unverified_chain(self) -> list[bytes]:
            """Returns raw certificate chain provided by the other
            end of the SSL channel as a list of DER-encoded bytes.
            """

class SSLErrorNumber(enum.IntEnum):
    """An enumeration."""

    SSL_ERROR_EOF = 8
    SSL_ERROR_INVALID_ERROR_CODE = 10
    SSL_ERROR_SSL = 1
    SSL_ERROR_SYSCALL = 5
    SSL_ERROR_WANT_CONNECT = 7
    SSL_ERROR_WANT_READ = 2
    SSL_ERROR_WANT_WRITE = 3
    SSL_ERROR_WANT_X509_LOOKUP = 4
    SSL_ERROR_ZERO_RETURN = 6

SSL_ERROR_EOF: SSLErrorNumber  # undocumented
SSL_ERROR_INVALID_ERROR_CODE: SSLErrorNumber  # undocumented
SSL_ERROR_SSL: SSLErrorNumber  # undocumented
SSL_ERROR_SYSCALL: SSLErrorNumber  # undocumented
SSL_ERROR_WANT_CONNECT: SSLErrorNumber  # undocumented
SSL_ERROR_WANT_READ: SSLErrorNumber  # undocumented
SSL_ERROR_WANT_WRITE: SSLErrorNumber  # undocumented
SSL_ERROR_WANT_X509_LOOKUP: SSLErrorNumber  # undocumented
SSL_ERROR_ZERO_RETURN: SSLErrorNumber  # undocumented

def get_protocol_name(protocol_code: int) -> str: ...

PEM_FOOTER: str
PEM_HEADER: str
SOCK_STREAM: int
SOL_SOCKET: int
SO_TYPE: int
