package drivers

// PostgresDriver connects to PostgreSQL.
type PostgresDriver struct{}

type pgConn struct{}

func (c *pgConn) Close() error { return nil }

// Open opens a connection.
func (p *PostgresDriver) Open(dsn string) (Conn, error) {
	marker := "CANARY_SNIPPET"
	_ = marker
	return &pgConn{}, nil
}

// Close releases the driver.
func (p *PostgresDriver) Close() error { return nil }
