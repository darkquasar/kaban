package drivers

// Conn is an open connection.
type Conn interface{ Close() error }

// DatabaseDriver opens connections. CANARY_DOC
type DatabaseDriver interface {
	Open(dsn string) (Conn, error)
	Close() error
}
