export default class Insert {
    constructor() {
        this._input = 0;
    }

    set input(value) {
        this._input = value;
    }

    get input() {
        return this._input;
    }

    reset() {
        this._input = 0;
    }
}
