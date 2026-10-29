package drivers

// MemDriver keeps connections in memory.
type MemDriver struct{}

type memConn struct{}

func (c memConn) Close() error { return nil }

// Open opens an in-memory connection.
func (m MemDriver) Open(dsn string) (Conn, error) { return memConn{}, nil }

// Close releases the driver.
func (m MemDriver) Close() error { return nil }
