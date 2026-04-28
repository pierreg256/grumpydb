/** Base error class for GrumpyDB client errors. */
export class GrumpyError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "GrumpyError";
  }
}

/** TCP/TLS connection failure. */
export class ConnectionError extends GrumpyError {
  constructor(message: string) {
    super(message);
    this.name = "ConnectionError";
  }
}

/** Authentication failure. */
export class AuthError extends GrumpyError {
  constructor(message: string) {
    super(message);
    this.name = "AuthError";
  }
}

/** Protocol parsing failure. */
export class ProtocolError extends GrumpyError {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

/** Server returned an error response. */
export class ServerError extends GrumpyError {
  constructor(message: string) {
    super(message);
    this.name = "ServerError";
  }
}
