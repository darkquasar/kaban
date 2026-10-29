package sqlite

import "example.com/drivers/drivers"

// Driver is a sibling-package implementer of drivers.DatabaseDriver.
type Driver struct{}

type conn struct{}

func (c *conn) Close() error { return nil }

// Open opens a connection.
func (d *Driver) Open(dsn string) (drivers.Conn, error) { return &conn{}, nil }

// Close releases the driver.
func (d *Driver) Close() error { return nil }
