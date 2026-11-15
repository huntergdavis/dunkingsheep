import React from 'react';

export default class ChunkErrorBoundary extends React.Component {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  render() {
    if (this.state.failed) return <div role="alert">
      <p>This page was updated. Reload to continue.</p>
      <button onClick={() => window.location.reload()}>Reload page</button>
    </div>;
    return this.props.children;
  }
}
