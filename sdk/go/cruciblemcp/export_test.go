package cruciblemcp

// SetWorkHook installs, or with nil removes, the capture work hook (see workHook) for the package's tests.
func SetWorkHook(h func(serialized, scrubbed int)) {
	if h == nil {
		workHook.Store(nil)
		return
	}
	workHook.Store(&h)
}
