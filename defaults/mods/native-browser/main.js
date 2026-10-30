import { View, div } from "gpui-kit";
export default class Browser extends View {
  render() { return div().p(16).child("This pane requires Herdr Browser on macOS."); }
}
