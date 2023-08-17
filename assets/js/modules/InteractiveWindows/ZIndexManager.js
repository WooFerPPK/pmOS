export class ZIndexManager {
    constructor() {
        this.counter = 1;
    }

    getTopZIndex() {
        return this.counter++;
    }
}